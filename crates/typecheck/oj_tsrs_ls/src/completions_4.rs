// completions.go, lines 5489-6905.

use std::sync::Arc;
use std::sync::OnceLock;

use tsrs_ast::{
    self as ast, FindAncestorResult, Kind, Node, NodeFactory, NodeFactoryHooks, NodeFlags,
    SourceFile, Symbol, SymbolFlags, TokenFlags,
};
use tsrs_checker::{Checker, Flags, LiteralValue, TypeFlags};
use tsrs_compiler::Program;
use tsrs_core::context::Context;
use tsrs_core::{
    CompilerOptions, P, TextChange, TextRange, apply_bulk_edits, compare_text_ranges, goslices,
    tspath,
};
use tsrs_lsproto as lsproto;
use tsrs_printer::{
    self as printer, ChangeTrackerWriter, EmitContext, EmitFlags, EmitTextWriter, Printer,
    PrinterOptions,
};
use tsrs_scanner as scanner;

use crate::astnav;
use crate::autoimport::{self, ImportAdder};
use crate::completions::*;
use crate::format;
use crate::languageservice::LanguageService;
use crate::lsconv;
use crate::lsutil::{self, FormatCodeSettings, QuotePreference, UserPreferences};
use crate::utilities::{is_in_string, new_case_clause_tracker};

impl LanguageService {
    // completions.go:5489
    pub fn resolve_completion_item(
        &self,
        ctx: &Context,
        item: lsproto::CompletionItem,
        data: Option<&lsproto::CompletionItemData>,
    ) -> Result<lsproto::CompletionItem, lsproto::Error> {
        let Some(data) = data else {
            return Err(lsproto::Error::new("completion item data is nil"));
        };

        let (program, file) = self.try_get_program_and_file(&data.file_name);
        let Some(file) = file else {
            return Err(lsproto::Error::new(format!("file not found: {}", data.file_name)));
        };
        let Some(file) =
            source_file_for_supplemental_file_index(file, data.supplemental_file_index)
        else {
            return Err(lsproto::Error::new(format!(
                "supplemental source file index not found: {}",
                data.supplemental_file_index.unwrap()
            )));
        };

        let mut checker_handle = program.get_type_checker_for_file(ctx, file);
        let checker: &mut Checker = &mut checker_handle;
        Ok(self.get_completion_item_details(
            ctx,
            program,
            checker,
            lsconv::snap_to_char_boundary(file.text(), u32::try_from(data.position).unwrap_or(0)),
            file,
            item,
            data,
        ))
    }
}

// completions.go:5512
fn get_completion_documentation_format(ctx: &Context) -> lsproto::MarkupKind {
    lsproto::preferred_markup_kind(
        &lsproto::get_client_capabilities(ctx)
            .text_document
            .completion
            .completion_item
            .documentation_format,
    )
}

impl LanguageService {
    // completions.go:5516
    fn get_completion_item_details(
        &self,
        ctx: &Context,
        program: &'static Program,
        checker: &mut Checker,
        position: u32,
        file: P<SourceFile>,
        mut item: lsproto::CompletionItem,
        data: &lsproto::CompletionItemData,
    ) -> lsproto::CompletionItem {
        let doc_format = get_completion_documentation_format(ctx);
        let (context_token, previous_token) = get_relevant_tokens(position, file);
        if is_in_string(file, position, previous_token) {
            return self.get_string_literal_completion_details(
                ctx,
                checker,
                item,
                &data.name,
                file,
                position,
                context_token,
                doc_format,
            );
        }

        if let Some(auto_import) = &data.auto_import {
            if data.is_import_statement_completion {
                return item;
            }
            // Auto-imports in content-mapped files are evaluated eagerly so edits outside
            // of verbatim spans can cause the completion item to be filtered out entirely.
            // Only real files take this code path, so the final Edits() is guaranteed ok.
            let (edits, description, _) =
                (autoimport::Fix { auto_import_fix: auto_import.clone(), ..Default::default() })
                    .edits(
                        ctx,
                        file,
                        program.options(),
                        self.format_options(),
                        &self.converters,
                        self.user_preferences(),
                        false, /*include_symbol_name_in_description*/
                    );
            item.additional_text_edits = Some(edits);
            item.detail = str_ptr_to(&description);
            return item;
        }

        // Compute all the completion symbols again.
        let symbol_completion =
            self.get_symbol_completion_from_item_data(ctx, checker, file, position, data);
        let preferences = self.user_preferences().clone();

        if let Some(request) = symbol_completion.request {
            match request {
                CompletionData::JSDocTagName
                | CompletionData::JSDocTag
                | CompletionData::JSDocParameterName(_) => {
                    create_simple_details(item, &data.name, doc_format)
                }
                CompletionData::Keyword(request) => {
                    if request.keyword_completions.iter().any(|c| c.label == data.name) {
                        return create_simple_details(item, &data.name, doc_format);
                    }
                    item
                }
                CompletionData::Data(_) => {
                    panic!("Unexpected completion data type: *ls.completionDataData")
                }
            }
        } else if let Some(symbol_details) = symbol_completion.symbol {
            self.create_completion_details_for_symbol(
                item,
                symbol_details.symbol,
                checker,
                symbol_details.location,
                position,
                doc_format,
            )
        } else if let Some(literal) = symbol_completion.literal {
            create_simple_details(
                item,
                &completion_name_for_literal(file, &preferences, &literal),
                doc_format,
            )
        } else if symbol_completion.cases.is_some() {
            item
        } else {
            // Didn't find a symbol with this name.  See if we can find a keyword instead.
            if all_keyword_completions().iter().any(|c| c.label == data.name) {
                return create_simple_details(item, &data.name, doc_format);
            }
            item
        }
    }
}

// completions.go:5609
#[derive(Default)]
struct DetailsData {
    symbol: Option<SymbolDetails>,
    request: Option<CompletionData>,
    literal: Option<LiteralValue>,
    cases: Option<()>,
}

// completions.go:5616
#[expect(
    dead_code,
    reason = "Go reads only symbol and location too; the rest wait for `!!! fill in additionalTextEdits from code actions` (completions.go:5712)"
)]
struct SymbolDetails {
    symbol: P<Symbol>,
    location: P<Node>,
    origin: Option<SymbolOriginInfo>,
    previous_token: Option<P<Node>>,
    context_token: Option<P<Node>>,
    jsx_initializer: JsxInitializer,
    is_type_only_location: bool,
}

impl LanguageService {
    // completions.go:5626
    fn get_symbol_completion_from_item_data(
        &self,
        ctx: &Context,
        ch: &mut Checker,
        file: P<SourceFile>,
        position: u32,
        item_data: &lsproto::CompletionItemData,
    ) -> DetailsData {
        if item_data.source == SOURCE_SWITCH_CASES {
            return DetailsData { cases: Some(()), ..Default::default() };
        }

        let preferences = self.user_preferences().clone();
        let completion_data = match self.get_completion_data(
            ctx,
            ch,
            file,
            position,
            &preferences,
            true, /*forItemResolve*/
        ) {
            Ok(completion_data) => completion_data,
            Err(err) => panic!("{}", err.message),
        };

        let Some(completion_data) = completion_data else {
            return DetailsData::default();
        };

        let data = match completion_data {
            CompletionData::Data(data) => data,
            other => {
                return DetailsData { request: Some(other), ..Default::default() };
            }
        };

        let literal = data
            .literals
            .iter()
            .find(|literal| {
                completion_name_for_literal(file, &preferences, literal) == item_data.name
            })
            .copied();
        if literal.is_some() {
            return DetailsData { literal, ..Default::default() };
        }

        // Find the symbol with the matching entry name.
        // We don't need to perform character checks here because we're only comparing the
        // name against 'entryName' (which is known to be good), not building a new
        // completion entry.
        if let Some(result) = data.symbols.iter().enumerate().find_map(|(index, &symbol)| {
            let origin = data.symbol_to_origin_info_map.get(&index);
            let (display_name, _) = get_completion_entry_display_name_for_symbol(
                file,
                &preferences,
                symbol,
                origin,
                data.completion_kind,
                data.is_jsx_identifier_expected,
            );
            if display_name == item_data.name
                && (item_data.source == COMPLETION_SOURCE_CLASS_MEMBER_SNIPPET
                    && symbol.flags().intersects(SymbolFlags::ClassMember)
                    || item_data.source == COMPLETION_SOURCE_OBJECT_LITERAL_METHOD_SNIPPET
                        && symbol.flags().intersects(SymbolFlags::Property | SymbolFlags::Method)
                    || get_source_from_origin(origin) == item_data.source
                    || item_data.source == COMPLETION_SOURCE_OBJECT_LITERAL_MEMBER_WITH_COMMA)
            {
                return Some(DetailsData {
                    symbol: Some(SymbolDetails {
                        symbol,
                        location: data.location,
                        origin: origin.cloned(),
                        previous_token: data.previous_token,
                        context_token: data.context_token,
                        jsx_initializer: data.jsx_initializer,
                        is_type_only_location: data.is_type_only_location,
                    }),
                    ..Default::default()
                });
            }

            None
        }) {
            return result;
        }
        DetailsData::default()
    }
}

// completions.go:5698
fn create_simple_details(
    item: lsproto::CompletionItem,
    name: &str,
    doc_format: lsproto::MarkupKind,
) -> lsproto::CompletionItem {
    create_completion_details(item, name, "" /*documentation*/, doc_format)
}

// completions.go:5706
pub(crate) fn create_completion_details(
    mut item: lsproto::CompletionItem,
    detail: &str,
    documentation: &str,
    doc_format: lsproto::MarkupKind,
) -> lsproto::CompletionItem {
    // !!! fill in additionalTextEdits from code actions
    if item.detail.is_none() && !detail.is_empty() {
        item.detail = Some(detail.to_string());
    }
    if !documentation.is_empty() {
        item.documentation = Some(lsproto::StringOrMarkupContent {
            markup_content: Some(lsproto::MarkupContent {
                kind: doc_format,
                value: documentation.to_string(),
            }),
            ..Default::default()
        });
    }
    item
}

// completions.go:5727
#[expect(
    dead_code,
    reason = "Go has no user yet either: `!!! fill in additionalTextEdits from code actions` (completions.go:5712)"
)]
struct CodeAction {
    // Description of the code action to display in the UI of the editor
    description: String,
    // Text changes to apply to each file as part of the code action
    changes: Vec<lsproto::TextEdit>,
}

impl LanguageService {
    // completions.go:5734
    pub(crate) fn create_completion_details_for_symbol(
        &self,
        item: lsproto::CompletionItem,
        symbol: P<Symbol>,
        checker: &mut Checker,
        location: P<Node>,
        _position: u32,
        doc_format: lsproto::MarkupKind,
    ) -> lsproto::CompletionItem {
        let (quick_info, documentation, _, _) = self.get_quick_info_and_documentation_for_symbol(
            checker,
            Some(symbol),
            location,
            doc_format,
            None,
            false, /*vsCapability*/
        );
        create_completion_details(item, &quick_info, &documentation, doc_format)
    }

    // completions.go:5746
    pub(crate) fn get_import_statement_completion_info(
        &self,
        context_token: P<Node>,
        source_file: P<SourceFile>,
    ) -> ImportStatementCompletionInfo {
        let mut result = ImportStatementCompletionInfo::default();
        let mut candidate: Option<P<Node>> = None;
        let parent = context_token.parent().unwrap();
        if ast::is_import_equals_declaration(parent) {
            // import Foo |
            // import Foo f|
            let last_token = lsutil::get_last_token(Some(parent), source_file);
            if context_token.kind() == Kind::Identifier && last_token != Some(context_token) {
                result.keyword_completion = Kind::FromKeyword;
                result.is_keyword_only_completion = true;
            } else {
                if context_token.kind() != Kind::TypeKeyword {
                    result.keyword_completion = Kind::TypeKeyword;
                }
                if is_module_specifier_missing_or_empty(Some(
                    parent.as_import_equals_declaration().module_reference,
                )) {
                    candidate = Some(parent);
                }
            }
        } else if could_be_type_only_import_specifier(parent, context_token)
            && can_complete_from_named_bindings(parent.parent().unwrap())
        {
            candidate = Some(parent);
        } else if ast::is_named_imports(parent) || ast::is_namespace_import(parent) {
            if !parent.parent().unwrap().is_type_only()
                && (context_token.kind() == Kind::OpenBraceToken
                    || context_token.kind() == Kind::ImportKeyword
                    || context_token.kind() == Kind::CommaToken)
            {
                result.keyword_completion = Kind::TypeKeyword;
            }
            if can_complete_from_named_bindings(parent) {
                // At `import { ... } |` or `import * as Foo |`, the only possible completion is `from`
                if context_token.kind() == Kind::CloseBraceToken
                    || context_token.kind() == Kind::Identifier
                {
                    result.is_keyword_only_completion = true;
                    result.keyword_completion = Kind::FromKeyword;
                } else {
                    candidate = parent.parent().unwrap().parent();
                }
            }
        } else if ast::is_export_declaration(parent) && context_token.kind() == Kind::AsteriskToken
            || ast::is_named_exports(parent) && context_token.kind() == Kind::CloseBraceToken
        {
            result.is_keyword_only_completion = true;
            result.keyword_completion = Kind::FromKeyword;
        } else if context_token.kind() == Kind::ImportKeyword {
            if ast::is_source_file(parent) {
                // A lone import keyword with nothing following it does not parse as a statement at all
                result.keyword_completion = Kind::TypeKeyword;
                candidate = Some(context_token);
            } else if ast::is_import_declaration(parent) {
                // `import s| from`
                result.keyword_completion = Kind::TypeKeyword;
                if is_module_specifier_missing_or_empty(parent.module_specifier()) {
                    candidate = Some(parent);
                }
            }
        }

        if let Some(candidate) = candidate {
            result.is_new_identifier_location = true;
            result.replacement_span =
                self.get_single_line_replacement_span_for_import_completion_node(candidate);
            result.could_be_type_only_import_specifier =
                could_be_type_only_import_specifier(candidate, context_token);
            if ast::is_import_declaration(candidate) {
                if let Some(import_clause) = candidate.import_clause() {
                    result.is_top_level_type_only = import_clause.is_type_only();
                }
            } else if candidate.kind() == Kind::ImportEqualsDeclaration {
                result.is_top_level_type_only = candidate.is_type_only();
            }
        } else {
            result.is_new_identifier_location = result.keyword_completion == Kind::TypeKeyword;
        }
        result
    }

    // completions.go:5821
    fn get_single_line_replacement_span_for_import_completion_node(
        &self,
        node: P<Node>,
    ) -> Option<lsproto::Range> {
        let mut node = node;
        // node is ImportDeclaration | ImportEqualsDeclaration | ImportSpecifier | JSDocImportTag | Token<SyntaxKind.ImportKeyword>
        if let Some(ancestor) = ast::find_ancestor(node, |n| {
            ast::is_import_declaration(n)
                || ast::is_import_equals_declaration(n)
                || ast::is_jsdoc_import_tag(n)
        }) {
            node = ancestor;
        }
        let source_file = ast::get_source_file_of_node(node).unwrap();
        // Use token position (excluding JSDoc/trivia) instead of node.Pos() to avoid including JSDoc comments
        let token_pos =
            scanner::get_token_pos_of_node(node, source_file, false /*includeJSDoc*/);
        if printer::get_lines_between_positions(source_file, token_pos.into(), node.end()).is_same()
        {
            let (lsp_range, fidelity) = self.create_lsp_range_from_node(node, source_file);
            if !fidelity.is_exact() {
                return None;
            }
            return Some(lsp_range);
        }

        assert!(
            !(node.kind() == Kind::ImportKeyword || node.kind() == Kind::ImportSpecifier),
            "ImportKeyword was necessarily on one line; ImportSpecifier was necessarily parented in an ImportDeclaration"
        );

        // Guess which point in the import might actually be a later statement parsed as part of the import
        // during parser recovery - either in the middle of named imports, or the module specifier.
        let potential_split_point: P<Node> =
            if node.kind() == Kind::ImportDeclaration || node.kind() == Kind::JSDocImportTag {
                let mut specifier: Option<P<Node>> = None;
                if let Some(import_clause) = node.import_clause() {
                    specifier = get_potentially_invalid_import_specifier(
                        import_clause.as_import_clause().named_bindings,
                    );
                }

                if let Some(specifier) = specifier {
                    specifier
                } else {
                    node.module_specifier().unwrap()
                }
            } else {
                node.as_import_equals_declaration().module_reference
            };

        let without_module_specifier = TextRange::new(
            scanner::get_token_pos_of_node(
                lsutil::get_first_token(node, source_file).unwrap(),
                source_file,
                false,
            ),
            potential_split_point
                .pos()
                .expect_source("parsed module specifiers have source positions"),
        );
        // The module specifier/reference was previously found to be missing, empty, or
        // not a string literal - in this last case, it's likely that statement on a following
        // line was parsed as the module specifier of a partially-typed import, e.g.
        //   import Foo|
        //   interface Blah {}
        // This appears to be a multiline-import, and editors can't replace multiple lines.
        // But if everything but the "module specifier" is on one line, by this point we can
        // assume that the "module specifier" is actually just another statement, and return
        // the single-line range of the import excluding that probable statement.
        if printer::get_lines_between_positions(
            source_file,
            without_module_specifier.pos(),
            without_module_specifier.end(),
        )
        .is_same()
        {
            let (lsp_range, fidelity) = self.create_lsp_range_from_bounds(
                without_module_specifier.pos().expect_source("import ranges have source positions"),
                without_module_specifier.end().expect_source("import ranges have source positions"),
                source_file,
            );
            if !fidelity.is_exact() {
                return None;
            }
            return Some(lsp_range);
        }
        None
    }
}

// completions.go:5879
fn could_be_type_only_import_specifier(import_specifier: P<Node>, context_token: P<Node>) -> bool {
    ast::is_import_specifier(import_specifier)
        && (import_specifier.is_type_only()
            || import_specifier.name() == Some(context_token)
                && is_type_keyword_token_or_identifier(context_token))
}

// completions.go:5883
fn can_complete_from_named_bindings(named_bindings: P<Node>) -> bool {
    let import_clause = named_bindings.parent().unwrap();
    if !is_module_specifier_missing_or_empty(import_clause.parent().unwrap().module_specifier())
        || import_clause.name().is_some()
    {
        return false;
    }
    if ast::is_named_imports(named_bindings) {
        // We can only complete on named imports if there are no other named imports already,
        // but parser recovery sometimes puts later statements in the named imports list, so
        // we try to only consider the probably-valid ones.
        let invalid_named_import = get_potentially_invalid_import_specifier(Some(named_bindings));
        let elements = named_bindings.elements();
        let valid_imports = match invalid_named_import {
            Some(invalid_named_import) => {
                elements.iter().position(|&element| element == invalid_named_import)
            }
            None => Some(elements.len()),
        };

        return valid_imports.is_some_and(|count| count < 2);
    }
    true
}

// Tries to identify the first named import that is not really a named import, but rather
// just parser recovery for a situation like:
//
//	import { Foo|
//	interface Bar {}
//
// in which `Foo`, `interface`, and `Bar` are all parsed as import specifiers. The caller
// will also check if this token is on a separate line from the rest of the import.
// completions.go:5911
fn get_potentially_invalid_import_specifier(named_bindings: Option<P<Node>>) -> Option<P<Node>> {
    let named_bindings = named_bindings?;
    if named_bindings.kind() != Kind::NamedImports {
        return None;
    }
    named_bindings.elements().iter().copied().find(|&e| {
        e.property_name().is_none()
            && lsutil::is_non_contextual_keyword(scanner::string_to_token(e.name().unwrap().text()))
            && astnav::find_preceding_token(
                ast::get_source_file_of_node(named_bindings).unwrap(),
                e.name().unwrap().pos().expect_source("parsed import names have source positions"),
            )
            .unwrap()
            .kind()
                != Kind::CommaToken
    })
}

// completions.go:5921
fn is_module_specifier_missing_or_empty(specifier: Option<P<Node>>) -> bool {
    if ast::node_is_missing(specifier) {
        return true;
    }
    let mut node = specifier.unwrap();
    if ast::is_external_module_reference(node) {
        node = node.expression().unwrap();
    }
    if !ast::is_string_literal_like(node) {
        return true;
    }
    node.text().is_empty()
}

// completions.go:5935
pub(crate) fn has_doc_comment(file: P<SourceFile>, position: u32) -> bool {
    let token = astnav::get_token_at_position(file, position);
    ast::find_ancestor(token, |n| n.is_jsdoc()).is_some()
}

// Get the corresponding JSDocTag node if the position is in a JSDoc comment
// completions.go:5941
pub(crate) fn get_jsdoc_tag_at_position(node: P<Node>, position: u32) -> Option<P<Node>> {
    ast::find_ancestor_or_quit(node, |n| {
        if ast::is_jsdoc_tag(n) && n.loc().contains_inclusive(position.into()) {
            return FindAncestorResult::True;
        }
        if n.is_jsdoc() {
            return FindAncestorResult::Quit;
        }
        FindAncestorResult::False
    })
}

// completions.go:5953
pub(crate) fn try_get_type_expression_from_tag(tag: P<Node>) -> Option<P<Node>> {
    if is_tag_with_type_expression(tag) {
        let type_expression = if ast::is_jsdoc_template_tag(tag) {
            tag.as_jsdoc_template_tag().constraint
        } else {
            tag.type_expression()
        };
        if let Some(type_expression) = type_expression
            && type_expression.kind() == Kind::JSDocTypeExpression
        {
            return Some(type_expression);
        }
    }
    if ast::is_jsdoc_augments_tag(tag) || ast::is_jsdoc_implements_tag(tag) {
        return Some(tag.class_name());
    }
    None
}

// completions.go:5971
fn is_tag_with_type_expression(tag: P<Node>) -> bool {
    match tag.kind() {
        Kind::JSDocParameterTag
        | Kind::JSDocPropertyTag
        | Kind::JSDocReturnTag
        | Kind::JSDocTypeTag
        | Kind::JSDocTypedefTag
        | Kind::JSDocThrowsTag
        | Kind::JSDocSatisfiesTag => true,
        Kind::JSDocTemplateTag => tag.as_jsdoc_template_tag().constraint.is_some(),
        _ => false,
    }
}

impl LanguageService {
    // completions.go:5983
    pub(crate) fn js_doc_completion_info(
        &self,
        ctx: &Context,
        position: u32,
        file: P<SourceFile>,
        mut items: Vec<CompletionItem>,
    ) -> CompletionList {
        let default_commit_characters =
            get_default_commit_characters(false /*isNewIdentifierLocation*/);
        let item_defaults = self.set_item_defaults(
            ctx,
            position,
            file,
            &mut items,
            Some(&default_commit_characters),
            None, /*optionalReplacementSpan*/
        );
        CompletionList { is_incomplete: false, item_defaults, items, ..Default::default() }
    }
}

// completions.go:6005
const JS_DOC_TAG_NAMES: &[&str] = &[
    "abstract",
    "access",
    "alias",
    "argument",
    "async",
    "augments",
    "author",
    "borrows",
    "callback",
    "class",
    "classdesc",
    "constant",
    "constructor",
    "constructs",
    "copyright",
    "default",
    "deprecated",
    "description",
    "emits",
    "enum",
    "event",
    "example",
    "exports",
    "extends",
    "external",
    "field",
    "file",
    "fileoverview",
    "fires",
    "function",
    "generator",
    "global",
    "hideconstructor",
    "host",
    "ignore",
    "implements",
    "import",
    "inheritdoc",
    "inner",
    "instance",
    "interface",
    "kind",
    "lends",
    "license",
    "link",
    "linkcode",
    "linkplain",
    "listens",
    "member",
    "memberof",
    "method",
    "mixes",
    "module",
    "name",
    "namespace",
    "overload",
    "override",
    "package",
    "param",
    "private",
    "prop",
    "property",
    "protected",
    "public",
    "readonly",
    "requires",
    "returns",
    "satisfies",
    "see",
    "since",
    "static",
    "summary",
    "template",
    "this",
    "throws",
    "todo",
    "tutorial",
    "type",
    "typedef",
    "var",
    "variation",
    "version",
    "virtual",
    "yields",
];

// completions.go:6092
fn js_doc_tag_name_completion_items() -> &'static [lsproto::CompletionItem] {
    static ITEMS: OnceLock<Vec<lsproto::CompletionItem>> = OnceLock::new();
    ITEMS.get_or_init(|| {
        let mut items = Vec::with_capacity(JS_DOC_TAG_NAMES.len());
        items.extend(JS_DOC_TAG_NAMES.iter().map(|tag_name| lsproto::CompletionItem {
            label: tag_name.to_string(),
            kind: Some(lsproto::CompletionItemKind::KEYWORD),
            sort_text: Some(SORT_TEXT_LOCATION_PRIORITY.to_string()),
            ..Default::default()
        }));
        items
    })
}

// completions.go:6105
fn js_doc_tag_completion_items() -> &'static [lsproto::CompletionItem] {
    static ITEMS: OnceLock<Vec<lsproto::CompletionItem>> = OnceLock::new();
    ITEMS.get_or_init(|| {
        let mut items = Vec::with_capacity(JS_DOC_TAG_NAMES.len());
        items.extend(JS_DOC_TAG_NAMES.iter().map(|tag_name| lsproto::CompletionItem {
            label: format!("@{tag_name}"),
            kind: Some(lsproto::CompletionItemKind::KEYWORD),
            sort_text: Some(SORT_TEXT_LOCATION_PRIORITY.to_string()),
            ..Default::default()
        }));
        items
    })
}

// completions.go:6118
pub(crate) fn get_jsdoc_tag_name_completions() -> Vec<CompletionItem> {
    clone_items(js_doc_tag_name_completion_items())
}

// completions.go:6122
pub(crate) fn get_jsdoc_tag_completions() -> Vec<CompletionItem> {
    clone_items(js_doc_tag_completion_items())
}

// completions.go:6126
pub(crate) fn get_jsdoc_parameter_completions(
    _ctx: &Context,
    file: P<SourceFile>,
    position: u32,
    type_checker: &mut Checker,
    options: P<CompilerOptions>,
    preferences: &UserPreferences,
    tag_name_only: bool,
) -> Vec<CompletionItem> {
    let current_token = astnav::get_token_at_position(file, position);
    if !ast::is_jsdoc_tag(current_token) && !current_token.is_jsdoc() {
        return Vec::new();
    }
    let js_doc =
        if current_token.is_jsdoc() { current_token } else { current_token.parent().unwrap() };
    if !js_doc.is_jsdoc() {
        return Vec::new();
    }
    let fun = js_doc.parent().unwrap();
    if !ast::is_function_like(fun) {
        return Vec::new();
    }

    let is_js = ast::is_source_file_js(file);
    // isSnippet := clientSupportsItemSnippet(clientOptions)
    let is_snippet = false; // !!! need snippet printer
    let mut param_tag_count = 0;
    let mut tags: &[P<Node>] = &[];
    if let Some(t) = js_doc.as_jsdoc().tags {
        tags = t.nodes();
    }
    param_tag_count += tags
        .iter()
        .filter(|&&tag| {
            ast::is_jsdoc_parameter_tag(tag)
                && astnav::get_start_of_node(tag, file, false /*includeJSDoc*/) < position
                && ast::is_identifier(tag.name().unwrap())
        })
        .count();

    let mut result = Vec::new();
    let mut emit_context: Option<P<EmitContext>> = None;
    for (param_index, &param) in fun.parameters().iter().enumerate() {
        if param_index < param_tag_count {
            // This parameter is already annotated.
            continue;
        }
        if ast::is_identifier(param.name().unwrap()) {
            // Named parameter
            let mut tabstop_counter = 1;
            let param_name = param.name().unwrap().text();
            let mut display_text = get_jsdoc_param_annotation(
                &mut emit_context,
                param_name,
                param.initializer(),
                param.as_parameter_declaration().dot_dot_dot_token(),
                is_js,
                /*isObject*/ false,
                /*isSnippet*/ false,
                type_checker,
                options,
                preferences,
                Some(&mut tabstop_counter),
            );
            let mut snippet_text = String::new();
            if is_snippet {
                snippet_text = get_jsdoc_param_annotation(
                    &mut emit_context,
                    param_name,
                    param.initializer(),
                    param.as_parameter_declaration().dot_dot_dot_token(),
                    is_js,
                    /*isObject*/ false,
                    /*isSnippet*/ true,
                    type_checker,
                    options,
                    preferences,
                    Some(&mut tabstop_counter),
                );
            }
            if tag_name_only {
                // Remove `@`
                display_text = display_text[1..].to_string();
                if !snippet_text.is_empty() {
                    snippet_text = snippet_text[1..].to_string();
                }
            }

            result.push(CompletionItem::new(lsproto::CompletionItem {
                label: display_text,
                kind: Some(lsproto::CompletionItemKind::VARIABLE),
                sort_text: Some(SORT_TEXT_LOCATION_PRIORITY.to_string()),
                insert_text: str_ptr_to(&snippet_text),
                insert_text_format: if is_snippet {
                    Some(lsproto::InsertTextFormat::SNIPPET)
                } else {
                    None
                },
                ..Default::default()
            }));
        } else if param_index == param_tag_count {
            // Destructuring parameter; do it positionally
            let param_path = format!("param{param_index}");
            let display_text_result = generate_jsdoc_param_tags_for_destructuring(
                &mut emit_context,
                &param_path,
                param.name().unwrap(),
                param.initializer(),
                param.as_parameter_declaration().dot_dot_dot_token(),
                is_js,
                /*isSnippet*/ false,
                type_checker,
                options,
                preferences,
            );
            let mut snippet_text = String::new();
            if is_snippet {
                let snippet_text_result = generate_jsdoc_param_tags_for_destructuring(
                    &mut emit_context,
                    &param_path,
                    param.name().unwrap(),
                    param.initializer(),
                    param.as_parameter_declaration().dot_dot_dot_token(),
                    is_js,
                    /*isSnippet*/ true,
                    type_checker,
                    options,
                    preferences,
                );
                snippet_text = snippet_text_result
                    .join(&(options.new_line.get_new_line_character().to_string() + "* "));
            }
            let mut display_text = display_text_result
                .join(&(options.new_line.get_new_line_character().to_string() + "* "));
            if tag_name_only {
                // Remove `@`
                display_text = display_text.strip_prefix('@').unwrap_or(&display_text).to_string();
                snippet_text = snippet_text.strip_prefix('@').unwrap_or(&snippet_text).to_string();
            }
            result.push(CompletionItem::new(lsproto::CompletionItem {
                label: display_text,
                kind: Some(lsproto::CompletionItemKind::VARIABLE),
                sort_text: Some(SORT_TEXT_LOCATION_PRIORITY.to_string()),
                insert_text: str_ptr_to(&snippet_text),
                insert_text_format: if is_snippet {
                    Some(lsproto::InsertTextFormat::SNIPPET)
                } else {
                    None
                },
                ..Default::default()
            }));
        }
    }
    result
}

// completions.go:6269
fn get_jsdoc_param_annotation(
    emit_context: &mut Option<P<EmitContext>>,
    param_name: &str,
    initializer: Option<P<Node>>,
    dot_dot_dot_token: Option<P<Node>>,
    is_js: bool,
    is_object: bool,
    is_snippet: bool,
    type_checker: &mut Checker,
    _options: P<CompilerOptions>,
    preferences: &UserPreferences,
    mut tabstop_counter: Option<&mut usize>,
) -> String {
    if is_snippet {
        assert!(tabstop_counter.is_some());
    }
    let mut param_name = param_name.to_string();
    if let Some(initializer) = initializer {
        param_name = get_jsdoc_param_name_with_initializer(&param_name, initializer);
    }
    if is_snippet {
        param_name = escape_snippet_text(&param_name);
    }
    if is_js {
        let mut t = "*".to_string();
        if is_object {
            assert!(
                dot_dot_dot_token.is_none(),
                "Cannot annotate a rest parameter with type 'object'."
            );
            t = "object".to_string();
        } else {
            if let Some(initializer) = initializer {
                let inferred_type =
                    type_checker.get_type_at_location(initializer.parent().unwrap());
                if !inferred_type.flags().intersects(TypeFlags::Any | TypeFlags::Void) {
                    let file = ast::get_source_file_of_node(initializer).unwrap();
                    let quote_preference = lsutil::get_quote_preference(file, preferences);
                    let builder_flags = if quote_preference == QuotePreference::Single {
                        Flags::UseSingleQuotesForStringLiteralType
                    } else {
                        Flags::None
                    };
                    let type_node = type_checker.type_to_type_node(
                        inferred_type,
                        ast::find_ancestor(initializer, ast::is_function_like),
                        builder_flags,
                        None, /*idToSymbol*/
                    );
                    if let Some(type_node) = type_node {
                        let emit_context =
                            *emit_context.get_or_insert_with(printer::new_emit_context);
                        // !!! snippet p
                        let mut p = printer::new_printer(
                            PrinterOptions {
                                remove_comments: true,
                                // !!!
                                // Module: options.Module,
                                // ModuleResolution: options.ModuleResolution,
                                // Target: options.Target,
                                ..Default::default()
                            },
                            printer::PrintHandlers::default(),
                            Some(emit_context),
                        );
                        emit_context.set_emit_flags(type_node, EmitFlags::SingleLine);
                        t = p.emit(type_node, Some(file));
                    }
                }
            }
            if is_snippet && t == "*" {
                let counter = tabstop_counter.as_deref_mut().unwrap();
                let tabstop = *counter;
                *counter += 1;
                t = format!("${{{tabstop}:{t}}}");
            }
        }
        let dot_dot_dot = if !is_object && dot_dot_dot_token.is_some() { "..." } else { "" };
        let mut description = String::new();
        if is_snippet {
            let counter = tabstop_counter.as_deref_mut().unwrap();
            let tabstop = *counter;
            *counter += 1;
            description = format!("${{{tabstop}}}");
        }
        format!("@param {{{dot_dot_dot}{t}}} {param_name} {description}")
    } else {
        let mut description = String::new();
        if is_snippet {
            let counter = tabstop_counter.unwrap();
            let tabstop = *counter;
            *counter += 1;
            description = format!("${{{tabstop}}}");
        }
        format!("@param {param_name} {description}")
    }
}

// completions.go:6352
fn get_jsdoc_param_name_with_initializer(param_name: &str, initializer: P<Node>) -> String {
    let initializer_text = scanner::get_text_of_node(initializer);
    let initializer_text = initializer_text.trim();
    if initializer_text.contains('\n') || initializer_text.len() > 80 {
        return format!("[{param_name}]");
    }
    format!("[{param_name}={initializer_text}]")
}

// completions.go:6360
fn generate_jsdoc_param_tags_for_destructuring(
    emit_context: &mut Option<P<EmitContext>>,
    path: &str,
    pattern: P<Node>,
    initializer: Option<P<Node>>,
    dot_dot_dot_token: Option<P<Node>>,
    is_js: bool,
    is_snippet: bool,
    type_checker: &mut Checker,
    options: P<CompilerOptions>,
    preferences: &UserPreferences,
) -> Vec<String> {
    let mut tabstop_counter = 1;
    if !is_js {
        return vec![get_jsdoc_param_annotation(
            emit_context,
            path,
            initializer,
            dot_dot_dot_token,
            is_js,
            /*isObject*/ false,
            is_snippet,
            type_checker,
            options,
            preferences,
            Some(&mut tabstop_counter),
        )];
    }
    js_doc_param_pattern_worker(
        emit_context,
        path,
        pattern,
        initializer,
        dot_dot_dot_token,
        is_js,
        is_snippet,
        type_checker,
        options,
        preferences,
        &mut tabstop_counter,
    )
}

// completions.go:6400
fn js_doc_param_pattern_worker(
    emit_context: &mut Option<P<EmitContext>>,
    path: &str,
    pattern: P<Node>,
    initializer: Option<P<Node>>,
    dot_dot_dot_token: Option<P<Node>>,
    is_js: bool,
    is_snippet: bool,
    type_checker: &mut Checker,
    options: P<CompilerOptions>,
    preferences: &UserPreferences,
    counter: &mut usize,
) -> Vec<String> {
    if ast::is_object_binding_pattern(pattern) && dot_dot_dot_token.is_none() {
        let mut child_counter = *counter;
        let root_param = get_jsdoc_param_annotation(
            emit_context,
            path,
            initializer,
            dot_dot_dot_token,
            is_js,
            /*isObject*/ true,
            is_snippet,
            type_checker,
            options,
            preferences,
            Some(&mut child_counter),
        );
        let mut child_tags: Vec<String> = Vec::new();
        for &element in pattern.elements() {
            let element_tags = js_doc_param_element_worker(
                emit_context,
                path,
                element,
                initializer,
                dot_dot_dot_token,
                is_js,
                is_snippet,
                type_checker,
                options,
                preferences,
                &mut child_counter,
            );
            if element_tags.is_empty() {
                child_tags = Vec::new();
                break;
            }
            child_tags.extend(element_tags);
        }
        if !child_tags.is_empty() {
            *counter = child_counter;
            let mut result = vec![root_param];
            result.extend(child_tags);
            return result;
        }
    }
    vec![get_jsdoc_param_annotation(
        emit_context,
        path,
        initializer,
        dot_dot_dot_token,
        is_js,
        /*isObject*/ false,
        is_snippet,
        type_checker,
        options,
        preferences,
        Some(counter),
    )]
}

// Assumes binding element is inside object binding pattern.
// We can't deeply annotate an array binding pattern.
// completions.go:6469
fn js_doc_param_element_worker(
    emit_context: &mut Option<P<EmitContext>>,
    path: &str,
    element: P<Node>,
    _initializer: Option<P<Node>>,
    _dot_dot_dot_token: Option<P<Node>>,
    is_js: bool,
    is_snippet: bool,
    type_checker: &mut Checker,
    options: P<CompilerOptions>,
    preferences: &UserPreferences,
    counter: &mut usize,
) -> Vec<String> {
    if ast::is_identifier(element.name().unwrap()) {
        // `{ b }` or `{ b: newB }`
        let property_name = if let Some(property_name) = element.property_name() {
            ast::try_get_text_of_property_name(property_name).unwrap_or_default()
        } else {
            element.name().unwrap().text().to_string()
        };
        if property_name.is_empty() {
            return Vec::new();
        }
        let param_name = format!("{path}.{property_name}");
        return vec![get_jsdoc_param_annotation(
            emit_context,
            &param_name,
            element.initializer(),
            element.as_binding_element().dot_dot_dot_token(),
            is_js,
            /*isObject*/ false,
            is_snippet,
            type_checker,
            options,
            preferences,
            Some(counter),
        )];
    } else if let Some(property_name_node) = element.property_name() {
        // `{ b: {...} }` or `{ b: [...] }`
        let property_name =
            ast::try_get_text_of_property_name(property_name_node).unwrap_or_default();
        if property_name.is_empty() {
            return Vec::new();
        }
        return js_doc_param_pattern_worker(
            emit_context,
            &format!("{path}.{property_name}"),
            element.name().unwrap(),
            element.initializer(),
            element.as_binding_element().dot_dot_dot_token(),
            is_js,
            is_snippet,
            type_checker,
            options,
            preferences,
            counter,
        );
    }
    Vec::new()
}

// completions.go:6527
pub(crate) fn get_jsdoc_parameter_name_completions(tag: P<Node>) -> Vec<CompletionItem> {
    if !ast::is_identifier(tag.name().unwrap()) {
        return Vec::new();
    }
    let name_thus_far = tag.name().unwrap().text();
    let js_doc = tag.parent().unwrap();
    let fn_ = js_doc.parent().unwrap();
    if !ast::is_function_like(fn_) {
        return Vec::new();
    }

    let mut tags: &[P<Node>] = &[];
    if let Some(t) = js_doc.as_jsdoc().tags {
        tags = t.nodes();
    }

    let mut result = Vec::new();
    result.extend(fn_.parameters().iter().filter_map(|&param| {
        if !ast::is_identifier(param.name().unwrap()) {
            return None;
        }

        let name = param.name().unwrap().text();
        if tags.iter().any(|&t| {
            t != tag
                && ast::is_jsdoc_parameter_tag(t)
                && ast::is_identifier(t.name().unwrap())
                && t.name().unwrap().text() == name
        }) || !name_thus_far.is_empty() && !name.starts_with(name_thus_far)
        {
            return None;
        }

        Some(CompletionItem::new(lsproto::CompletionItem {
            label: name.to_string(),
            kind: Some(lsproto::CompletionItemKind::VARIABLE),
            sort_text: Some(SORT_TEXT_LOCATION_PRIORITY.to_string()),
            ..Default::default()
        }))
    }));
    result
}

impl LanguageService {
    // completions.go:6568
    pub(crate) fn get_exhaustive_case_snippets(
        &self,
        ctx: &Context,
        case_block: P<Node>,
        file: P<SourceFile>,
        position: u32,
        options: P<CompilerOptions>,
        program: &'static Program,
        c: &mut Checker,
    ) -> Result<Option<lsproto::CompletionItem>, lsproto::Error> {
        let clauses = case_block.as_case_block().clauses.nodes();
        let switch_type =
            c.get_type_at_location(case_block.parent().unwrap().expression().unwrap());
        if switch_type.is_union() && switch_type.types().iter().all(|&t| is_literal(t)) {
            // Collect constant values in existing clauses.
            let mut tracker = new_case_clause_tracker(c, clauses);
            let target = options.get_emit_script_target();
            let quote_preference = lsutil::get_quote_preference(file, self.user_preferences());
            // Tolerate a nil import adder in untitled files.
            let mut import_adder: Option<Box<dyn ImportAdder>> = None;
            if !tspath::is_dynamic_file_name(file.file_name()) {
                let view = self.get_prepared_auto_import_view(file, c)?;
                // Go checks the view for nil; a prepared view is never nil.
                import_adder = Some(autoimport::new_import_adder(
                    ctx,
                    program,
                    c,
                    file,
                    view,
                    self.format_options(),
                    Arc::clone(&self.converters),
                    self.user_preferences().clone(),
                ));
            }

            let mut elements: Vec<P<Node>> = Vec::new();
            let factory = NodeFactory::new(NodeFactoryHooks::default());
            for &t in switch_type.types() {
                // Enums
                if t.is_enum_literal() {
                    assert!(t.symbol().is_some(), "An enum member type should have a symbol");
                    assert!(
                        t.symbol().unwrap().parent().is_some(),
                        "An enum member type should have a parent symbol (the enum symbol)"
                    );
                    // Filter existing enums by their values
                    let mut enum_value: Option<LiteralValue> = None;
                    if let Some(value_declaration) = t.symbol().unwrap().value_declaration() {
                        enum_value = c.get_constant_value(value_declaration);
                    }
                    if let Some(enum_value) = enum_value {
                        if tracker.has_value(&tracker_value(&enum_value)) {
                            continue;
                        }
                        tracker.add_value(tracker_value(&enum_value));
                    }
                    let Some(type_node) = autoimport::type_to_auto_importable_type_node(
                        c,
                        import_adder.as_deref_mut(),
                        t,
                        case_block,
                    ) else {
                        return Ok(None);
                    };
                    let Some(expr) =
                        type_node_to_expression(type_node, target, quote_preference, &factory)
                    else {
                        return Ok(None);
                    };
                    elements.push(expr);
                } else {
                    let value = t.as_literal_type().value().unwrap();
                    if !tracker.has_value(&tracker_value(&value)) {
                        // Literals
                        match value {
                            LiteralValue::BigInt(mut v) => {
                                let big_int = if v.negative {
                                    v.negative = false;
                                    factory.new_prefix_unary_expression(
                                        Kind::MinusToken,
                                        factory.new_big_int_literal(
                                            tsrs_core::alloc_str(&(v.string() + "n")),
                                            TokenFlags::None,
                                        ),
                                    )
                                } else {
                                    factory.new_big_int_literal(
                                        tsrs_core::alloc_str(&(v.string() + "n")),
                                        TokenFlags::None,
                                    )
                                };
                                elements.push(big_int);
                            }
                            LiteralValue::Number(v) => {
                                let number = if v.0 < 0.0 {
                                    factory.new_prefix_unary_expression(
                                        Kind::MinusToken,
                                        factory.new_numeric_literal(
                                            tsrs_core::alloc_str(&v.abs().string()),
                                            TokenFlags::None,
                                        ),
                                    )
                                } else {
                                    factory.new_numeric_literal(
                                        tsrs_core::alloc_str(&v.string()),
                                        TokenFlags::None,
                                    )
                                };
                                elements.push(number);
                            }
                            LiteralValue::String(v) => {
                                let literal = factory.new_string_literal(
                                    v,
                                    if quote_preference == QuotePreference::Single {
                                        TokenFlags::SingleQuote
                                    } else {
                                        TokenFlags::None
                                    },
                                );
                                elements.push(literal);
                            }
                            LiteralValue::Boolean(_) => {}
                        }
                    }
                }
            }
            if elements.is_empty() {
                return Ok(None);
            }

            let new_clauses: Vec<P<Node>> = elements
                .iter()
                .map(|&element| {
                    factory.new_case_or_default_clause(
                        Kind::CaseClause,
                        Some(element),
                        factory.new_node_list(Vec::new()),
                    )
                })
                .collect();
            let new_line_char = self.format_options().new_line_character.clone();
            let mut printer = create_snippet_printer(
                PrinterOptions {
                    remove_comments: true,
                    new_line: tsrs_core::get_new_line_kind(&new_line_char),
                    ..Default::default()
                },
                None, /*emitContext*/
            );
            let mut texts: Vec<String> = Vec::with_capacity(new_clauses.len());
            texts.extend(new_clauses.iter().enumerate().map(|(i, &clause)| {
                if client_supports_item_snippet(ctx) {
                    format!("{}${}", printer.print_and_format_node(ctx, clause, file), i + 1)
                } else {
                    printer.print_unescaped_node(clause)
                }
            }));
            let insert_text = texts.join(&new_line_char);

            let first_clause = printer.print_unescaped_node(new_clauses[0]);
            let name = first_clause + " ...";

            let mut additional_text_edits: Option<Vec<lsproto::TextEdit>> = None;
            if let Some(import_adder) = import_adder.as_mut() {
                let edits = import_adder.edits();
                if !edits.is_empty() {
                    additional_text_edits = Some(edits);
                }
            }

            return Ok(Some(lsproto::CompletionItem {
                label: name.clone(),
                kind: Some(lsproto::CompletionItemKind::SNIPPET),
                sort_text: Some(SORT_TEXT_GLOBALS_OR_KEYWORDS.to_string()),
                insert_text: str_ptr_to(&insert_text),
                additional_text_edits,
                insert_text_format: if client_supports_item_snippet(ctx) {
                    Some(lsproto::InsertTextFormat::SNIPPET)
                } else {
                    None
                },
                data: Some(lsproto::CompletionItemData {
                    file_name: file.original_file_name().to_string(),
                    position: i32::try_from(position)
                        .expect("completion position exceeds protocol i32"),
                    supplemental_file_index: supplemental_file_index(file).map(|index| {
                        i32::try_from(index).expect("supplemental file index exceeds protocol i32")
                    }),
                    name,
                    source: COMPLETION_SOURCE_SWITCH_CASES.to_string(),
                    ..Default::default()
                }),
                ..Default::default()
            }));
        }
        Ok(None)
    }
}

// completions.go:6706
fn type_node_to_expression(
    type_node: P<Node>,
    target: tsrs_core::ScriptTarget,
    quote_preference: QuotePreference,
    factory: &NodeFactory,
) -> Option<P<Node>> {
    match type_node.kind() {
        Kind::TypeReference => {
            let type_name = type_node.as_type_reference_node().type_name;
            Some(entity_name_to_expression(type_name, target, quote_preference, factory))
        }
        Kind::IndexedAccessType => {
            let object_expression = type_node_to_expression(
                type_node.as_indexed_access_type_node().object_type,
                target,
                quote_preference,
                factory,
            );
            let index_expression = type_node_to_expression(
                type_node.as_indexed_access_type_node().index_type,
                target,
                quote_preference,
                factory,
            );
            if let (Some(object_expression), Some(index_expression)) =
                (object_expression, index_expression)
            {
                return Some(factory.new_element_access_expression(
                    object_expression,
                    None, /*questionDotToken*/
                    index_expression,
                    NodeFlags::None,
                ));
            }
            None
        }
        Kind::LiteralType => {
            let literal = type_node.as_literal_type_node().literal;
            match literal.kind() {
                Kind::StringLiteral => {
                    let expr = factory.new_string_literal(
                        literal.text(),
                        if quote_preference == QuotePreference::Single {
                            TokenFlags::SingleQuote
                        } else {
                            TokenFlags::None
                        },
                    );
                    Some(expr)
                }
                Kind::NumericLiteral => {
                    let expr = factory.new_numeric_literal(
                        literal.text(),
                        literal.as_numeric_literal().token_flags(),
                    );
                    Some(expr)
                }
                _ => None,
            }
        }
        Kind::ParenthesizedType => {
            let expr = type_node_to_expression(
                type_node.as_parenthesized_type_node().type_,
                target,
                quote_preference,
                factory,
            )?;
            if ast::is_identifier(expr) {
                return Some(expr);
            }
            Some(factory.new_parenthesized_expression(expr))
        }
        Kind::TypeQuery => Some(entity_name_to_expression(
            type_node.as_type_query_node().expr_name,
            target,
            quote_preference,
            factory,
        )),
        Kind::ImportType => {
            panic!(
                "Debug Failure. We should not get an import type after calling 'typeToAutoImportableTypeNode'."
            );
        }
        _ => None,
    }
}

// completions.go:6768
#[expect(
    clippy::only_used_in_recursion,
    reason = "The pinned Go function also carries these parameters through recursion without otherwise reading them."
)]
fn entity_name_to_expression(
    entity_name: P<Node>,
    target: tsrs_core::ScriptTarget,
    quote_preference: QuotePreference,
    factory: &NodeFactory,
) -> P<Node> {
    if ast::is_identifier(entity_name) {
        return entity_name;
    }
    factory.new_property_access_expression(
        entity_name_to_expression(
            entity_name.as_qualified_name().left,
            target,
            quote_preference,
            factory,
        ),
        None, /*questionDotToken*/
        entity_name.as_qualified_name().right,
        NodeFlags::None,
    )
}

// completions.go:6785 (Go keeps `baseWriter` and `writer.ChangeTrackerWriter` as the same pointer; here the base
// writer lives inside `writer`)
pub(crate) struct SnippetPrinter {
    pub(crate) emit_context: P<EmitContext>,
    printer: Printer,
    writer: SnippetEmitTextWriter,
    pub(crate) factory: NodeFactory,
}

impl SnippetPrinter {
    // completions.go:6802
    fn print_unescaped_node(&mut self, node: P<Node>) -> String {
        self.writer.escapes.clear();
        self.writer.clear();
        self.printer.write(
            node,
            None, /*sourceFile*/
            &mut self.writer,
            None, /*sourceMapGenerator*/
        );
        self.writer.string()
    }

    // completions.go:6809
    fn print_and_format_node(
        &mut self,
        ctx: &Context,
        node: P<Node>,
        source_file: P<SourceFile>,
    ) -> String {
        let settings = format::get_format_code_settings_from_context(&format_context(ctx));
        self.print_and_format_node_with_settings(ctx, node, source_file, settings)
    }

    // completions.go:6813
    pub(crate) fn print_and_format_node_with_settings(
        &mut self,
        ctx: &Context,
        node: P<Node>,
        source_file: P<SourceFile>,
        format_options: FormatCodeSettings,
    ) -> String {
        let text = self.print_unescaped_node(node);
        let node_with_pos = self.writer.base.assign_positions_to_node(node, &self.factory);
        let synthetic_file = printer::create_synthetic_source_file(
            &self.factory,
            node_with_pos,
            &text,
            source_file.parse_options().clone(),
        );
        let new_line = format_options.new_line_character.clone();
        let fctx =
            format::with_format_code_settings(&format_context(ctx), format_options, &new_line);
        let changes = format::format_node_given_indentation(
            &fctx,
            node_with_pos,
            synthetic_file,
            source_file.language_variant(),
            0, /*initialIndentation*/
            0, /*delta*/
        );

        let mut all_changes = changes;
        if !self.writer.escapes.is_empty() {
            all_changes.extend(self.writer.escapes.iter().cloned());
            goslices::sort_func(&mut all_changes, |a, b| {
                compare_text_ranges(a.text_range, b.text_range)
            });
        }

        apply_bulk_edits(synthetic_file.text(), &all_changes)
    }
}

// completions.go:6836
pub(crate) fn create_snippet_printer(
    options: PrinterOptions,
    emit_context: Option<P<EmitContext>>,
) -> SnippetPrinter {
    let emit_context = emit_context.unwrap_or_else(printer::new_emit_context);
    let base_writer =
        printer::new_change_tracker_writer(options.new_line.get_new_line_character(), None);
    let printer =
        printer::new_printer(options, base_writer.get_print_handlers(), Some(emit_context));
    let writer = SnippetEmitTextWriter { base: base_writer, escapes: Vec::new() };
    SnippetPrinter {
        emit_context,
        printer,
        writer,
        factory: emit_context.factory.as_node_factory().clone(),
    }
}

// Override base writer methods to perform snippet escaping.
// completions.go:6855
struct SnippetEmitTextWriter {
    base: ChangeTrackerWriter,
    escapes: Vec<TextChange>,
}

impl SnippetEmitTextWriter {
    // The formatter/scanner will have issues with snippet-escaped text,
    // so instead of writing the escaped text directly to the writer,
    // generate a set of changes that can be applied to the unescaped text
    // to escape it post-formatting.
    // completions.go:6888
    fn escaping_write(&mut self, s: &str, write: impl FnOnce(&mut ChangeTrackerWriter)) {
        let escaped = escape_snippet_text(s);
        if escaped == s {
            write(&mut self.base);
        } else {
            let start = self.base.get_text_pos();
            write(&mut self.base);
            let end = self.base.get_text_pos();
            self.escapes
                .push(TextChange { new_text: escaped, text_range: TextRange::new(start, end) });
        }
    }
}

impl EmitTextWriter for SnippetEmitTextWriter {
    // completions.go:6860
    fn write(&mut self, s: &str) {
        self.escaping_write(s, |w| w.write(s));
    }

    // completions.go:6864
    fn write_comment(&mut self, text: &str) {
        self.escaping_write(text, |w| w.write_comment(text));
    }

    // completions.go:6868
    fn write_string_literal(&mut self, text: &str) {
        self.escaping_write(text, |w| w.write_string_literal(text));
    }

    // completions.go:6872
    fn write_parameter(&mut self, text: &str) {
        self.escaping_write(text, |w| w.write_parameter(text));
    }

    // completions.go:6876
    fn write_property(&mut self, text: &str) {
        self.escaping_write(text, |w| w.write_property(text));
    }

    // completions.go:6880
    fn write_symbol(&mut self, text: &str, symbol: P<Symbol>) {
        self.escaping_write(text, |w| w.write_symbol(text, symbol));
    }

    // The methods Go promotes from the embedded *printer.ChangeTrackerWriter.
    fn write_trailing_semicolon(&mut self, text: &str) {
        self.base.write_trailing_semicolon(text);
    }
    fn write_keyword(&mut self, text: &str) {
        self.base.write_keyword(text);
    }
    fn write_operator(&mut self, text: &str) {
        self.base.write_operator(text);
    }
    fn write_punctuation(&mut self, text: &str) {
        self.base.write_punctuation(text);
    }
    fn write_space(&mut self, text: &str) {
        self.base.write_space(text);
    }
    fn write_line(&mut self) {
        self.base.write_line();
    }
    fn write_line_force(&mut self, force: bool) {
        self.base.write_line_force(force);
    }
    fn increase_indent(&mut self) {
        self.base.increase_indent();
    }
    fn decrease_indent(&mut self) {
        self.base.decrease_indent();
    }
    fn clear(&mut self) {
        self.base.clear();
    }
    fn string(&self) -> String {
        self.base.string()
    }
    fn raw_write(&mut self, s: &str) {
        self.base.raw_write(s);
    }
    fn write_literal(&mut self, s: &str) {
        self.base.write_literal(s);
    }
    fn get_text_pos(&self) -> u32 {
        self.base.get_text_pos()
    }
    fn get_line(&self) -> u32 {
        self.base.get_line()
    }
    fn get_column(&self) -> tsrs_core::UTF16Offset {
        self.base.get_column()
    }
    fn get_indent(&self) -> u32 {
        self.base.get_indent()
    }
    fn is_at_start_of_line(&self) -> bool {
        self.base.is_at_start_of_line()
    }
    fn has_trailing_comment(&self) -> bool {
        self.base.has_trailing_comment()
    }
    fn has_trailing_whitespace(&self) -> bool {
        self.base.has_trailing_whitespace()
    }
}
