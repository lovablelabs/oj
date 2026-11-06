// completions.go, lines 1-1818 (the rest is in completions_2.rs .. completions_4.rs).

use rustc_hash::{FxHashMap, FxHashSet};
use tsrs_ast::{
    self as ast, Kind, ModifierFlags, Node, NodeFlags, SourceFile, Symbol, SymbolFlags, SymbolId,
};
use tsrs_checker::{self as checker, Checker, ContextFlags, LiteralValue, Type};
use tsrs_core::context::Context;
use tsrs_core::{P, stringutil, tspath};
use tsrs_lsproto as lsproto;
use tsrs_scanner as scanner;

use crate::astnav;
use crate::autoimport;
use crate::format::{self, FormatContext};
use crate::languageservice::LanguageService;
use crate::lsutil::{self, FormatCodeSettings, UserPreferences};
use crate::spanmap::Feature;
use crate::utilities::{
    is_in_comment, is_in_right_side_of_internal_import_equals_declaration, is_in_string,
};

pub use crate::completions_2::*;
pub use crate::completions_3::*;
pub(crate) use crate::completions_4::*;

// completions.go:35 (Go compares with `errors.Is`; `is_err_needs_auto_imports` compares the message)
const ERR_NEEDS_AUTO_IMPORTS: &str = "completion list needs auto imports";

pub(crate) fn err_needs_auto_imports() -> lsproto::Error {
    lsproto::Error::new(ERR_NEEDS_AUTO_IMPORTS)
}

pub fn is_err_needs_auto_imports(err: &lsproto::Error) -> bool {
    err.message == ERR_NEEDS_AUTO_IMPORTS
}

// Go keeps the formatter settings as values of the request `context.Context` (`format.WithFormatCodeSettings`).
// The format package takes a `format::FormatContext` (format/api.rs); it travels as a value of the request
// context so that every Go `ctx` keeps carrying it.
fn with_format_code_settings(
    ctx: &Context,
    options: FormatCodeSettings,
    new_line: &str,
) -> Context {
    let base = format_context(ctx);
    ctx.with_value(format::with_format_code_settings(&base, options, new_line))
}

pub(crate) fn format_context(ctx: &Context) -> FormatContext {
    ctx.value::<FormatContext>().cloned().unwrap_or_default()
}

impl LanguageService {
    // completions.go:37
    pub fn provide_completion(
        &self,
        ctx: &Context,
        document_uri: &lsproto::DocumentUri,
        lsp_position: lsproto::Position,
        context: Option<&lsproto::CompletionContext>,
    ) -> Result<lsproto::CompletionResponse, lsproto::Error> {
        let (program, mut file) = self.get_program_and_file(document_uri);
        let mut trigger_character: Option<&str> = None;
        if let Some(context) = context {
            trigger_character = context.trigger_character.as_deref();
        }
        let ctx = &with_format_code_settings(
            ctx,
            self.format_options(),
            &self.format_options().new_line_character,
        );
        let positions = self.converters.from_lsp_position_for_source_file(
            file,
            lsp_position,
            Feature::Completion,
        );
        if positions.is_empty() || !positions[0].fidelity.is_exact() {
            // In a content-mapped file the cursor is outside a verbatim span, so any completion committed here
            // could not be applied to the original text. Offer nothing rather than edits at a bogus location.
            return Ok(lsproto::CompletionItemsOrListOrNull::default());
        }
        file = positions[0].script;
        let position = positions[0].position;
        let completion_list_internal = self.get_completions_at_position_internal(
            ctx,
            file,
            position,
            trigger_character,
            false, /*includeSymbols*/
        )?;
        let mut completion_list =
            ensure_item_data(file, position, completion_list_internal.map(CompletionList::to_lsp));
        if crate::lsconv::Script::span_map(&file).is_some() {
            self.filter_content_mapped_auto_imports(ctx, program, file, completion_list.as_mut());
        }
        Ok(lsproto::CompletionItemsOrListOrNull { list: completion_list, ..Default::default() })
    }

    // filterContentMappedAutoImports eagerly resolves auto-import edits for a content-mapped file and drops any
    // completion whose import edit cannot be placed entirely within verbatim spans (it would otherwise insert
    // an import into synthesized virtual code with no counterpart in the original file). Surviving auto-imports carry
    // their additional edits directly so the client applies correct original-text positions on commit.
    // completions.go:78
    fn filter_content_mapped_auto_imports(
        &self,
        ctx: &Context,
        program: &'static tsrs_compiler::Program,
        file: P<SourceFile>,
        list: Option<&mut lsproto::CompletionList>,
    ) {
        let Some(list) = list else {
            return;
        };
        let items = std::mem::take(&mut list.items);
        let mut filtered = Vec::with_capacity(items.len());
        filtered.extend(items.into_iter().filter_map(|mut item| {
            let Some(auto_import) = item.data.as_ref().and_then(|d| d.auto_import.clone()) else {
                return Some(item);
            };
            let (edits, description, ok) =
                (autoimport::Fix { auto_import_fix: auto_import, ..Default::default() }).edits(
                    ctx,
                    file,
                    program.options(),
                    self.format_options(),
                    &self.converters,
                    self.user_preferences(),
                    false, /*include_symbol_name_in_description*/
                );
            if !ok {
                return None;
            }
            item.additional_text_edits = Some(edits);
            item.detail = str_ptr_to(&description);
            Some(item)
        }));
        list.items = filtered;
    }

    // completions.go:106
    pub fn get_completions_at_position(
        &self,
        ctx: &Context,
        file: P<SourceFile>,
        position: u32,
        trigger_character: Option<&str>,
        include_symbols: bool,
    ) -> Result<Option<CompletionList>, lsproto::Error> {
        let position = crate::lsconv::snap_to_char_boundary(file.text(), position);
        self.get_completions_at_position_internal(
            ctx,
            file,
            position,
            trigger_character,
            include_symbols,
        )
    }
}

// completions.go:110 (Go embeds the `*lsproto.CompletionItem` pointer, never nil)
#[derive(Clone, Debug)]
pub struct CompletionItem {
    pub completion_item: lsproto::CompletionItem,
    pub symbol: Option<P<Symbol>>, // non-nil for symbol completions when IncludeSymbols is set; nil otherwise
}

impl std::ops::Deref for CompletionItem {
    type Target = lsproto::CompletionItem;
    fn deref(&self) -> &lsproto::CompletionItem {
        &self.completion_item
    }
}

impl std::ops::DerefMut for CompletionItem {
    fn deref_mut(&mut self) -> &mut lsproto::CompletionItem {
        &mut self.completion_item
    }
}

impl CompletionItem {
    pub(crate) fn new(completion_item: lsproto::CompletionItem) -> CompletionItem {
        CompletionItem { completion_item, symbol: None }
    }
}

// completions.go:115
#[derive(Clone, Debug, Default)]
pub struct CompletionList {
    pub is_incomplete: bool,
    pub(crate) item_defaults: Option<lsproto::CompletionItemDefaults>,
    pub(crate) apply_kind: Option<lsproto::CompletionItemApplyKinds>,
    pub items: Vec<CompletionItem>,
}

// completions.go:122
fn ensure_item_data(
    file: P<SourceFile>,
    pos: u32,
    list: Option<lsproto::CompletionList>,
) -> Option<lsproto::CompletionList> {
    let mut list = list?;
    for item in &mut list.items {
        if item.data.is_none() {
            item.data = Some(lsproto::CompletionItemData {
                file_name: file.original_file_name().to_string(),
                position: i32::try_from(pos).expect("completion position exceeds protocol i32"),
                supplemental_file_index: supplemental_file_index(file).map(|index| {
                    i32::try_from(index).expect("supplemental file index exceeds protocol i32")
                }),
                name: item.label.clone(),
                ..Default::default()
            });
        }
    }
    Some(list)
}

// completions.go:139
pub(crate) fn supplemental_file_index(file: P<SourceFile>) -> Option<usize> {
    let canonical = file.canonical_source_file()?;
    if let Some(i) =
        canonical.supplemental_source_files().iter().position(|&supplemental| supplemental == file)
    {
        return Some(i);
    }
    panic!("supplemental source file is not linked from its canonical source file");
}

// completions.go:152
pub(crate) fn source_file_for_supplemental_file_index(
    file: P<SourceFile>,
    index: Option<i32>,
) -> Option<P<SourceFile>> {
    let Some(index) = index else {
        return Some(file);
    };
    let supplemental = file.supplemental_source_files();
    let index = usize::try_from(index).ok()?;
    supplemental.get(index).copied()
}

// *completionDataData | *completionDataKeyword | *completionDataJSDocTagName | *completionDataJSDocTag | *completionDataJSDocParameterName
// completions.go:164
pub(crate) enum CompletionData {
    Data(Box<CompletionDataData>),
    Keyword(CompletionDataKeyword),
    JSDocTagName,
    JSDocTag,
    JSDocParameterName(CompletionDataJSDocParameterName),
}

// completions.go:166 (Go's isInSnippetScope field is never read)
pub(crate) struct CompletionDataData {
    pub(crate) symbols: Vec<P<Symbol>>,
    pub(crate) auto_imports: Vec<autoimport::FixAndExport>,
    pub(crate) completion_kind: CompletionKind,
    // Note that the presence of this alone doesn't mean that we need a conversion. Only do that if the completion is not an ordinary identifier.
    pub(crate) property_access_to_convert: Option<P<Node>>,
    pub(crate) is_new_identifier_location: bool,
    pub(crate) location: P<Node>,
    pub(crate) keyword_filters: KeywordCompletionFilters,
    pub(crate) literals: Vec<LiteralValue>,
    pub(crate) symbol_to_origin_info_map: FxHashMap<usize, SymbolOriginInfo>,
    pub(crate) symbol_to_sort_text_map: FxHashMap<SymbolId, SortText>,
    pub(crate) recommended_completion: Option<P<Symbol>>,
    pub(crate) previous_token: Option<P<Node>>,
    pub(crate) context_token: Option<P<Node>>,
    pub(crate) jsx_initializer: JsxInitializer,
    pub(crate) inside_jsdoc_tag_type_expression: bool,
    pub(crate) is_type_only_location: bool,
    // In JSX tag name and attribute names, identifiers like "my-tag" or "aria-name" is valid identifier.
    pub(crate) is_jsx_identifier_expected: bool,
    pub(crate) is_right_of_open_tag: bool,
    pub(crate) is_right_of_dot_or_question_dot: bool,
    pub(crate) import_statement_completion: Option<ImportStatementCompletionInfo>, // !!!
    pub(crate) has_unresolved_auto_imports: bool,                                  // !!!
    // flags CompletionInfoFlags // !!!
    pub(crate) default_commit_characters: Option<Vec<String>>,
}

impl CompletionDataData {
    // Go's zero-valued `&completionDataData{...}` literal (string_completions.go builds one with a few fields).
    pub(crate) fn new(location: P<Node>) -> CompletionDataData {
        CompletionDataData {
            symbols: Vec::new(),
            auto_imports: Vec::new(),
            completion_kind: CompletionKind::None,
            property_access_to_convert: None,
            is_new_identifier_location: false,
            location,
            keyword_filters: KeywordCompletionFilters::None,
            literals: Vec::new(),
            symbol_to_origin_info_map: FxHashMap::default(),
            symbol_to_sort_text_map: FxHashMap::default(),
            recommended_completion: None,
            previous_token: None,
            context_token: None,
            jsx_initializer: JsxInitializer::default(),
            inside_jsdoc_tag_type_expression: false,
            is_type_only_location: false,
            is_jsx_identifier_expected: false,
            is_right_of_open_tag: false,
            is_right_of_dot_or_question_dot: false,
            import_statement_completion: None,
            has_unresolved_auto_imports: false,
            default_commit_characters: None,
        }
    }
}

// completions.go:195
pub(crate) struct CompletionDataKeyword {
    pub(crate) keyword_completions: Vec<CompletionItem>,
    is_new_identifier_location: bool,
}

// completions.go:204
pub(crate) struct CompletionDataJSDocParameterName {
    tag: P<Node>,
}

// completions.go:208
#[derive(Clone, Default)]
pub struct ImportStatementCompletionInfo {
    pub(crate) is_keyword_only_completion: bool,
    pub(crate) keyword_completion: Kind, // TokenKind
    pub(crate) is_new_identifier_location: bool,
    pub(crate) is_top_level_type_only: bool,
    pub(crate) could_be_type_only_import_specifier: bool,
    pub(crate) replacement_span: Option<lsproto::Range>,
}

// If we're after the `=` sign but no identifier has been typed yet,
// value will be `true` but initializer will be `nil`.
// completions.go:219
#[derive(Clone, Copy, Default)]
pub struct JsxInitializer {
    pub(crate) is_initializer: bool,
    pub(crate) initializer: Option<P<Node>>,
}

// completions.go:224
#[repr(i32)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum KeywordCompletionFilters {
    None = 0,                         // No keywords
    All = 1,                          // Every possible kewyord
    ClassElementKeywords = 2,         // Keywords inside class body
    InterfaceElementKeywords = 3,     // Keywords inside interface body
    ConstructorParameterKeywords = 4, // Keywords at constructor parameter
    FunctionLikeBodyKeywords = 5,     // Keywords at function like body
    TypeAssertionKeywords = 6,
    TypeKeywords = 7,
    TypeKeyword = 8, // Literally just `type`
}

impl KeywordCompletionFilters {
    pub(crate) const LAST: KeywordCompletionFilters = KeywordCompletionFilters::TypeKeyword;
}

// completions.go:239
fn keyword_filters_from_syntax_kind(keyword_completion: Kind) -> KeywordCompletionFilters {
    match keyword_completion {
        Kind::TypeKeyword => KeywordCompletionFilters::TypeKeyword,
        _ => panic!(
            "Unknown mapping from ast.Kind `Kind{keyword_completion:?}` to KeywordCompletionFilters"
        ),
    }
}

// completions.go:248
#[repr(i32)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum CompletionKind {
    None,
    ObjectPropertyDeclaration,
    Global,
    PropertyAccess,
    MemberLike,
    String,
}

// completions.go:259
pub const COMPLETION_TRIGGER_CHARACTERS: [&str; 10] =
    [".", "\"", "'", "`", "/", "@", "<", "#", " ", "*"];

// All commit characters, valid when `isNewIdentifierLocation` is false.
// completions.go:262
pub(crate) const ALL_COMMIT_CHARACTERS: [&str; 3] = [".", ",", ";"];

// Commit characters valid at expression positions where we could be inside a parameter list.
// completions.go:265
pub(crate) const NO_COMMA_COMMIT_CHARACTERS: [&str; 2] = [".", ";"];

// completions.go:267
pub(crate) const EMPTY_COMMIT_CHARACTERS: [&str; 0] = [];

pub(crate) fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(std::string::ToString::to_string).collect()
}

// completions.go:269 (`type SortText string`)
pub(crate) type SortText = String;

// completions.go:272
const SORT_TEXT_LOCAL_DECLARATION_PRIORITY: &str = "10";
pub(crate) const SORT_TEXT_LOCATION_PRIORITY: &str = "11";
const SORT_TEXT_OPTIONAL_MEMBER: &str = "12";
const SORT_TEXT_MEMBER_DECLARED_BY_SPREAD_ASSIGNMENT: &str = "13";
const SORT_TEXT_SUGGESTED_CLASS_MEMBERS: &str = "14";
pub(crate) const SORT_TEXT_GLOBALS_OR_KEYWORDS: &str = "15";
pub(crate) const SORT_TEXT_AUTO_IMPORT_SUGGESTIONS: &str = "16";
pub(crate) const SORT_TEXT_JAVASCRIPT_IDENTIFIERS: &str = "18";

// completions.go:283
pub(crate) fn deprecate_sort_text(original: &str) -> SortText {
    format!("z{original}")
}

// completions.go:287
fn object_literal_property_sort_text(
    preset_sort_text: &str,
    symbol_display_name: &str,
) -> SortText {
    format!("{preset_sort_text}\x00{symbol_display_name}\x00")
}

// completions.go:291
pub(crate) fn sort_below(original: &str) -> SortText {
    format!("{original}1")
}

bitflags::bitflags! {
    // completions.go:295
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
    #[expect(non_camel_case_types, reason = "Keep the upstream bitflags type name.")]
    pub struct symbolOriginInfoKind: u32 {
        const ThisType = 1 << 0;
        const SymbolMember = 1 << 1;
        const Promise = 1 << 2;
        const Nullable = 1 << 3;
        const TypeOnlyAlias = 1 << 4;
        const ObjectLiteralMethod = 1 << 5;
        const Ignore = 1 << 6;
        const ComputedPropertyName = 1 << 7;
    }
}

// Go `data any` of symbolOriginInfo.
#[derive(Clone, Default)]
pub enum SymbolOriginData {
    #[default]
    None,
    ObjectLiteralMethod(SymbolOriginInfoObjectLiteralMethod),
    TypeOnlyAlias(SymbolOriginInfoTypeOnlyAlias),
    ComputedPropertyName(SymbolOriginInfoComputedPropertyName),
}

// completions.go:308 (Go's isDefaultExport, isFromPackageJson and fileName fields are never set or read)
#[derive(Clone, Default)]
pub struct SymbolOriginInfo {
    pub(crate) kind: symbolOriginInfoKind,
    pub(crate) data: SymbolOriginData,
}

impl SymbolOriginInfo {
    fn with_kind(kind: symbolOriginInfoKind) -> SymbolOriginInfo {
        SymbolOriginInfo { kind, ..Default::default() }
    }

    // completions.go:316
    pub(crate) fn symbol_name(&self) -> String {
        match &self.data {
            SymbolOriginData::ComputedPropertyName(data) => data.symbol_name.clone(),
            _ => panic!(
                "symbolOriginInfo: unknown data type for symbolName(): {}",
                self.data.go_type()
            ),
        }
    }

    // completions.go:331
    pub(crate) fn as_object_literal_method(&self) -> &SymbolOriginInfoObjectLiteralMethod {
        match &self.data {
            SymbolOriginData::ObjectLiteralMethod(data) => data,
            _ => panic!(
                "interface conversion: interface {{}} is {}, not *ls.symbolOriginInfoObjectLiteralMethod",
                self.data.go_type()
            ),
        }
    }
}

impl SymbolOriginData {
    fn go_type(&self) -> &'static str {
        match self {
            SymbolOriginData::None => "<nil>",
            SymbolOriginData::ObjectLiteralMethod(_) => "*ls.symbolOriginInfoObjectLiteralMethod",
            SymbolOriginData::TypeOnlyAlias(_) => "*ls.symbolOriginInfoTypeOnlyAlias",
            SymbolOriginData::ComputedPropertyName(_) => "*ls.symbolOriginInfoComputedPropertyName",
        }
    }
}

// completions.go:325
#[derive(Clone, Default)]
pub struct SymbolOriginInfoObjectLiteralMethod {
    pub(crate) insert_text: String,
    pub(crate) label_details: Option<lsproto::CompletionItemLabelDetails>,
    pub(crate) is_snippet: bool,
}

// completions.go:335
#[derive(Clone)]
pub struct SymbolOriginInfoTypeOnlyAlias {
    #[expect(
        dead_code,
        reason = "Go has no reader yet either: `!!! fill in additionalTextEdits from code actions` (completions.go:5712)"
    )]
    declaration: P<Node>,
}

// completions.go:339
#[derive(Clone)]
pub struct SymbolOriginInfoComputedPropertyName {
    symbol_name: String,
}

// Special values for `CompletionInfo['source']` used to disambiguate
// completion items with the same `name`. (Each completion item must
// have a unique name/source combination, because those two fields
// comprise `CompletionEntryIdentifier` in `getCompletionEntryDetails`.
//
// When the completion item is an auto-import suggestion, the source
// is the module specifier of the suggestion. To avoid collisions,
// the values here should not be a module specifier we would ever
// generate for an auto-import.
// completions.go:352
// Completions that require `this.` insertion text.
pub(crate) const COMPLETION_SOURCE_THIS_PROPERTY: &str = "ThisProperty/";
// Auto-import that comes attached to a class member snippet.
pub(crate) const COMPLETION_SOURCE_CLASS_MEMBER_SNIPPET: &str = "ClassMemberSnippet/";
// A type-only import that needs to be promoted in order to be used at the completion location.
pub(crate) const COMPLETION_SOURCE_TYPE_ONLY_ALIAS: &str = "TypeOnlyAlias/";
// Auto-import that comes attached to an object literal method snippet.
pub(crate) const COMPLETION_SOURCE_OBJECT_LITERAL_METHOD_SNIPPET: &str =
    "ObjectLiteralMethodSnippet/";
// Case completions for switch statements.
pub(crate) const COMPLETION_SOURCE_SWITCH_CASES: &str = "SwitchCases/";
// Completions for an object literal expression.
pub(crate) const COMPLETION_SOURCE_OBJECT_LITERAL_MEMBER_WITH_COMMA: &str =
    "ObjectLiteralMemberWithComma/";

// Value is set to false for global variables or completions from external module exports,
// true otherwise.
// completions.go:371 (Go map; insertion order here)
pub(crate) type UniqueNamesMap = tsrs_core::collections::OrderedMap<String, bool>;

// completions.go:376
#[derive(Clone, Copy, PartialEq, Eq)]
enum GlobalsSearch {
    Continue,
    Success,
    Fail,
}

impl CompletionList {
    // completions.go:384
    #[expect(
        clippy::wrong_self_convention,
        reason = "The method name and mutable receiver match the upstream Go operation."
    )]
    fn to_lsp(self) -> lsproto::CompletionList {
        let mut items = Vec::with_capacity(self.items.len());
        items.extend(self.items.into_iter().map(|entry| entry.completion_item));
        lsproto::CompletionList {
            is_incomplete: self.is_incomplete,
            item_defaults: self.item_defaults,
            apply_kind: self.apply_kind,
            items,
        }
    }
}

impl LanguageService {
    // completions.go:402
    fn get_completions_at_position_internal(
        &self,
        ctx: &Context,
        file: P<SourceFile>,
        position: u32,
        trigger_character: Option<&str>,
        include_symbols: bool,
    ) -> Result<Option<CompletionList>, lsproto::Error> {
        let (_, previous_token) = get_relevant_tokens(position, file);
        if let Some(trigger_character) = trigger_character
            && !is_in_string(file, position, previous_token)
            && !is_valid_trigger(file, trigger_character, previous_token, position)
        {
            return Ok(None);
        }

        if trigger_character == Some(" ") {
            // `isValidTrigger` ensures we are at `import |`
            if self.user_preferences().include_completions_for_import_statements.is_true() {
                return Ok(Some(CompletionList { is_incomplete: true, ..Default::default() }));
            }
            return Ok(None);
        }

        if let Some(js_doc_snippet_completion) =
            self.get_jsdoc_snippet_completion(ctx, file, position)
        {
            return Ok(Some(js_doc_snippet_completion));
        }

        let compiler_options = self.get_program().options();

        // !!! see if incomplete completion list and continue or clean

        let mut checker_handle = self.get_program().get_type_checker_for_file(ctx, file);
        let checker: &mut Checker = &mut checker_handle;

        let string_completions = self.get_string_literal_completions(
            ctx,
            file,
            position,
            previous_token,
            checker,
            compiler_options,
            include_symbols,
        );
        if string_completions.is_some() {
            return Ok(string_completions);
        }

        if let Some(previous_token) = previous_token
            && (previous_token.kind() == Kind::BreakKeyword
                || previous_token.kind() == Kind::ContinueKeyword
                || previous_token.kind() == Kind::Identifier)
            && ast::is_break_or_continue_statement(previous_token.parent().unwrap())
        {
            return Ok(self.get_label_completions_at_position(
                ctx,
                previous_token.parent().unwrap(),
                file,
                position,
                self.get_optional_replacement_span(Some(previous_token), file),
            ));
        }

        let preferences = self.user_preferences().clone();
        let data = self.get_completion_data(
            ctx,
            checker,
            file,
            position,
            &preferences,
            false, /*forItemResolve*/
        )?;
        let Some(data) = data else {
            return Ok(None);
        };

        match data {
            CompletionData::Data(mut data) => {
                let optional_replacement_span =
                    self.get_optional_replacement_span(Some(data.location), file);
                let response = self.completion_info_from_data(
                    ctx,
                    checker,
                    file,
                    compiler_options,
                    &mut data,
                    position,
                    optional_replacement_span,
                    include_symbols,
                )?;
                Ok(response)
            }
            CompletionData::Keyword(data) => {
                let optional_replacement_span =
                    self.get_optional_replacement_span(previous_token, file);
                Ok(Some(self.specific_keyword_completion_info(
                    ctx,
                    position,
                    file,
                    data.keyword_completions,
                    data.is_new_identifier_location,
                    optional_replacement_span,
                )))
            }
            CompletionData::JSDocTagName => {
                // If the current position is a jsDoc tag name, only tag names should be provided for completion
                let mut items = get_jsdoc_tag_name_completions();
                items.extend(get_jsdoc_parameter_completions(
                    ctx,
                    file,
                    position,
                    checker,
                    compiler_options,
                    &preferences,
                    true, /*tagNameOnly*/
                ));
                Ok(Some(self.js_doc_completion_info(ctx, position, file, items)))
            }
            CompletionData::JSDocTag => {
                // If the current position is a jsDoc tag, only tags should be provided for completion
                let mut items = get_jsdoc_tag_completions();
                items.extend(get_jsdoc_parameter_completions(
                    ctx,
                    file,
                    position,
                    checker,
                    compiler_options,
                    &preferences,
                    false, /*tagNameOnly*/
                ));
                Ok(Some(self.js_doc_completion_info(ctx, position, file, items)))
            }
            CompletionData::JSDocParameterName(data) => Ok(Some(self.js_doc_completion_info(
                ctx,
                position,
                file,
                get_jsdoc_parameter_name_completions(data.tag),
            ))),
        }
    }

    // completions.go:528
    pub(crate) fn get_completion_data(
        &self,
        ctx: &Context,
        type_checker: &mut Checker,
        file: P<SourceFile>,
        position: u32,
        preferences: &UserPreferences,
        for_item_resolve: bool,
    ) -> Result<Option<CompletionData>, lsproto::Error> {
        let in_checked_file = is_checked_file(file, &self.get_program().options());

        let mut current_token = astnav::get_token_at_position(file, position);

        let inside_comment = is_in_comment(file, position, current_token);

        let mut inside_jsdoc_tag_type_expression = false;
        let mut inside_js_doc_import_tag = false;
        if inside_comment.is_some() {
            if has_doc_comment(file, position) {
                if position > 0 && file.text().as_bytes()[(position - 1) as usize] == b'@' {
                    // The current position is next to the '@' sign, when no tag name being provided yet.
                    // Provide a full list of tag names
                    return Ok(Some(CompletionData::JSDocTagName));
                }
                // When completion is requested without "@", we will have check to make sure that
                // there are no comments prefix the request position. We will only allow "*" and space.
                // e.g
                //   /** |c| /*
                //
                //   /**
                //     |c|
                //    */
                //
                //   /**
                //    * |c|
                //    */
                //
                //   /**
                //    *         |c|
                //    */
                let line_start = format::get_line_start_position_for_position(position, file);
                let no_comment_prefix = !(file.text()[line_start as usize..position as usize]
                    .chars()
                    .into_iter()
                    .any(|r| {
                        !(stringutil::is_white_space_single_line(r as u32)
                            || r == '*'
                            || r == '/'
                            || r == '('
                            || r == ')'
                            || r == '|')
                    }));
                if no_comment_prefix {
                    return Ok(Some(CompletionData::JSDocTag));
                }
            }

            // Completion should work inside certain JSDoc tags. For example:
            //     /** @type {number | string} */
            // Completion should work in the brackets
            if let Some(tag) = get_jsdoc_tag_at_position(current_token, position) {
                let tag_name = tag.tag_name();
                if tag_name.pos().expect_source("parsed JSDoc tags have source positions")
                    <= position
                    && position
                        <= tag_name.end().expect_source("parsed JSDoc tags have source positions")
                {
                    return Ok(Some(CompletionData::JSDocTagName));
                }
                if ast::is_jsdoc_import_tag(tag) {
                    inside_js_doc_import_tag = true;
                } else {
                    if let Some(type_expression) = try_get_type_expression_from_tag(tag) {
                        current_token = astnav::get_token_at_position(file, position);
                        // Go checks currentToken for nil; GetTokenAtPosition never returns nil.
                        if !ast::is_declaration_name(current_token)
                            && (current_token.parent().unwrap().kind() != Kind::JSDocPropertyTag
                                || current_token.parent().unwrap().name() != Some(current_token))
                        {
                            // Use as type location if inside tag's type expression
                            inside_jsdoc_tag_type_expression =
                                is_currently_editing_node(type_expression, file, position);
                        }
                    }
                    if !inside_jsdoc_tag_type_expression
                        && ast::is_jsdoc_parameter_tag(tag)
                        && (ast::node_is_missing(tag.name())
                            || tag
                                .name()
                                .unwrap()
                                .pos()
                                .expect_source("parsed JSDoc names have source positions")
                                <= position
                                && position
                                    <= tag
                                        .name()
                                        .unwrap()
                                        .end()
                                        .expect_source("parsed JSDoc names have source positions"))
                    {
                        return Ok(Some(CompletionData::JSDocParameterName(
                            CompletionDataJSDocParameterName { tag },
                        )));
                    }
                }
            }

            if !inside_jsdoc_tag_type_expression && !inside_js_doc_import_tag {
                // Proceed if the current position is in JSDoc tag expression; otherwise it is a normal
                // comment or the plain text part of a JSDoc comment, so no completion should be available
                return Ok(None);
            }
        }

        // The decision to provide completion depends on the contextToken, which is determined through the previousToken.
        // Note: 'previousToken' (and thus 'contextToken') can be undefined if we are the beginning of the file
        let is_js_only_location = !inside_jsdoc_tag_type_expression
            && !inside_js_doc_import_tag
            && ast::is_source_file_js(file);
        let (mut context_token, previous_token) = get_relevant_tokens(position, file);

        // Find the node where completion is requested on.
        // Also determine whether we are trying to complete with members of that node
        // or attributes of a JSX tag.
        let mut node = current_token;
        let mut property_access_to_convert: Option<P<Node>> = None;
        let mut is_right_of_dot = false;
        let mut is_right_of_question_dot = false;
        let mut is_right_of_open_tag = false;
        let mut is_starting_close_tag = false;
        let mut jsx_initializer = JsxInitializer::default();
        let mut is_jsx_identifier_expected = false;
        let mut import_statement_completion: Option<ImportStatementCompletionInfo> = None;
        let mut location = astnav::get_touching_property_name(file, position);
        let mut keyword_filters = KeywordCompletionFilters::None;
        let mut is_new_identifier_location = false;
        // !!! flags := CompletionInfoFlagsNone
        let default_commit_characters: Option<Vec<String>> = None;

        if let Some(ct) = context_token {
            let import_statement_completion_info =
                self.get_import_statement_completion_info(ct, file);
            if import_statement_completion_info.keyword_completion != Kind::Unknown {
                if import_statement_completion_info.is_keyword_only_completion {
                    return Ok(Some(CompletionData::Keyword(CompletionDataKeyword {
                        keyword_completions: vec![CompletionItem::new(lsproto::CompletionItem {
                            label: scanner::token_to_string(
                                import_statement_completion_info.keyword_completion,
                            )
                            .to_string(),
                            kind: Some(lsproto::CompletionItemKind::KEYWORD),
                            sort_text: Some(SORT_TEXT_GLOBALS_OR_KEYWORDS.to_string()),
                            ..Default::default()
                        })],
                        is_new_identifier_location: import_statement_completion_info
                            .is_new_identifier_location,
                    })));
                }
                keyword_filters = keyword_filters_from_syntax_kind(
                    import_statement_completion_info.keyword_completion,
                );
            }
            if import_statement_completion_info.replacement_span.is_some()
                && preferences.include_completions_for_import_statements.is_true()
            {
                // !!! flags |= CompletionInfoFlags.IsImportStatementCompletion;
                is_new_identifier_location =
                    import_statement_completion_info.is_new_identifier_location;
                import_statement_completion = Some(import_statement_completion_info.clone());
            }
            // Bail out if this is a known invalid completion location.
            if import_statement_completion_info.replacement_span.is_none()
                && is_completion_list_blocker(
                    ct,
                    previous_token,
                    location,
                    file,
                    position,
                    type_checker,
                )
            {
                if keyword_filters != KeywordCompletionFilters::None {
                    let (is_new_identifier_location, _) =
                        compute_commit_characters_and_is_new_identifier(Some(ct), file, position);
                    return Ok(Some(CompletionData::Keyword(keyword_completion_data(
                        keyword_filters,
                        is_js_only_location,
                        is_new_identifier_location,
                    ))));
                }
                return Ok(None);
            }

            let mut parent = ct.parent();
            if ct.kind() == Kind::DotToken || ct.kind() == Kind::QuestionDotToken {
                is_right_of_dot = ct.kind() == Kind::DotToken;
                is_right_of_question_dot = ct.kind() == Kind::QuestionDotToken;
                let parent = parent.unwrap();
                match parent.kind() {
                    Kind::PropertyAccessExpression => {
                        property_access_to_convert = Some(parent);
                        node = parent.expression().unwrap();
                        let left_most_access_expression =
                            ast::get_leftmost_access_expression(parent);
                        #[expect(
                            clippy::suspicious_operation_groupings,
                            reason = "the parsed expression end is intentionally compared with the dot token start"
                        )]
                        if ast::node_is_missing(left_most_access_expression)
                            || ((ast::is_call_expression(node) || ast::is_function_like(node))
                                && node.end() == ct.pos()
                                && lsutil::get_last_child(node, file).unwrap().kind()
                                    != Kind::CloseParenToken)
                        {
                            // This is likely dot from incorrectly parsed expression and user is starting to write spread
                            // eg: Math.min(./**/)
                            // const x = function (./**/) {}
                            // ({./**/})
                            return Ok(None);
                        }
                    }
                    Kind::QualifiedName => {
                        node = parent.as_qualified_name().left;
                    }
                    Kind::ModuleDeclaration => {
                        node = parent.name().unwrap();
                    }
                    Kind::ImportType => {
                        node = parent;
                    }
                    Kind::MetaProperty => {
                        node = lsutil::get_first_token(parent, file).unwrap();
                        assert!(
                            !(node.kind() != Kind::ImportKeyword
                                && node.kind() != Kind::NewKeyword),
                            "Unexpected token kind: Kind{:?}",
                            node.kind()
                        );
                    }
                    _ => {
                        // There is nothing that precedes the dot, so this likely just a stray character
                        // or leading into a '...' token. Just bail out instead.
                        return Ok(None);
                    }
                }
            } else if import_statement_completion.is_none() {
                // <UI.Test /* completion position */ />
                // If the tagname is a property access expression, we will then walk up to the top most of property access expression.
                // Then, try to get a JSX container and its associated attributes type.
                if let Some(p) = parent
                    && p.kind() == Kind::PropertyAccessExpression
                {
                    context_token = Some(p);
                    parent = p.parent();
                }
                let ct = context_token.unwrap();
                let parent = parent.unwrap();

                // Fix location
                if parent == location {
                    match current_token.kind() {
                        Kind::GreaterThanToken => {
                            if parent.kind() == Kind::JsxElement
                                || parent.kind() == Kind::JsxOpeningElement
                            {
                                location = current_token;
                            }
                        }
                        Kind::LessThanSlashToken
                            if parent.kind() == Kind::JsxSelfClosingElement =>
                        {
                            location = current_token;
                        }
                        _ => {}
                    }
                }

                match parent.kind() {
                    Kind::JsxClosingElement => {
                        if ct.kind() == Kind::LessThanSlashToken {
                            is_starting_close_tag = true;
                            location = ct;
                        }
                    }
                    // Go: `case KindBinaryExpression: if !binaryExpressionMayBeOpenTag(...) { break }; fallthrough`
                    Kind::BinaryExpression
                    | Kind::JsxSelfClosingElement
                    | Kind::JsxElement
                    | Kind::JsxOpeningElement
                        if parent.kind() != Kind::BinaryExpression
                            || binary_expression_may_be_open_tag(parent) =>
                    {
                        is_jsx_identifier_expected = true;
                        if ct.kind() == Kind::LessThanToken {
                            is_right_of_open_tag = true;
                            location = ct;
                        }
                    }
                    Kind::JsxExpression | Kind::JsxSpreadAttribute => {
                        // First case is for `<div foo={true} [||] />` or `<div foo={true} [||] ></div>`,
                        // `parent` will be `{true}` and `previousToken` will be `}`.
                        // Second case is for `<div foo={true} t[||] ></div>`.
                        // Second case must not match for `<div foo={undefine[||]}></div>`.
                        let previous_token = previous_token.unwrap();
                        if previous_token.kind() == Kind::CloseBraceToken
                            || previous_token.kind() == Kind::Identifier
                                && previous_token.parent().unwrap().kind() == Kind::JsxAttribute
                        {
                            is_jsx_identifier_expected = true;
                        }
                    }
                    Kind::JsxAttribute => {
                        // For `<div className="x" [||] ></div>`, `parent` will be JsxAttribute and `previousToken` will be its initializer.
                        let previous_token = previous_token.unwrap();
                        if parent.initializer() == Some(previous_token)
                            && previous_token
                                .end()
                                .expect_source("parsed tokens have source positions")
                                < position
                        {
                            is_jsx_identifier_expected = true;
                        } else {
                            match previous_token.kind() {
                                Kind::EqualsToken => {
                                    jsx_initializer.is_initializer = true;
                                }
                                Kind::Identifier => {
                                    is_jsx_identifier_expected = true;
                                    // For `<div x=[|f/**/|]`, `parent` will be `x` and `previousToken.parent` will be `f` (which is its own JsxAttribute).
                                    // Note for `<div someBool f>` we don't want to treat this as a jsx inializer, instead it's the attribute name.
                                    if Some(parent) != previous_token.parent()
                                        && parent.initializer().is_none()
                                        && astnav::find_child_of_kind(
                                            parent,
                                            Kind::EqualsToken,
                                            file,
                                        )
                                        .is_some()
                                    {
                                        jsx_initializer.initializer = Some(previous_token);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        let is_type_only_location = inside_jsdoc_tag_type_expression
            || inside_js_doc_import_tag
            || import_statement_completion.is_some()
                && location.parent().is_some()
                && ast::is_type_only_import_or_export_declaration(location.parent().unwrap())
            || !is_context_token_value_location(context_token)
                && (is_possibly_type_argument_position(context_token, file, type_checker)
                    || ast::is_part_of_type_node(location)
                    || is_context_token_type_location(context_token));

        let mut s = GetCompletionDataState {
            file,
            position,
            preferences: preferences.clone(),
            for_item_resolve,
            in_checked_file,
            inside_jsdoc_tag_type_expression,
            context_token,
            previous_token,
            node,
            location,
            is_right_of_dot,
            is_right_of_question_dot,
            is_right_of_open_tag,
            import_statement_completion: import_statement_completion.clone(),
            is_type_only_location,
            completion_kind: CompletionKind::None,
            has_unresolved_auto_imports: false,
            // This also gets mutated in nested-functions after the return
            symbols: Vec::new(),
            auto_imports: Vec::new(),
            // Keys are indexes of `symbols`.
            symbol_to_origin_info_map: FxHashMap::default(),
            symbol_to_sort_text_map: FxHashMap::default(),
            seen_property_symbols: FxHashSet::default(),
            is_new_identifier_location,
            default_commit_characters,
            keyword_filters,
        };

        if is_right_of_dot || is_right_of_question_dot {
            s.get_type_script_member_symbols(type_checker);
        } else if is_right_of_open_tag {
            s.symbols = type_checker.get_jsx_intrinsic_tag_names_at(location).to_vec();
            check_each_defined(&s.symbols, "GetJsxIntrinsicTagNamesAt() should all be defined");
            s.try_get_global_symbols(self, ctx, type_checker)?;
            s.completion_kind = CompletionKind::Global;
            s.keyword_filters = KeywordCompletionFilters::None;
        } else if is_starting_close_tag {
            let tag_name = context_token
                .unwrap()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .as_jsx_element()
                .opening_element
                .tag_name();
            let tag_symbol = type_checker.get_symbol_at_location_exported(tag_name);
            if let Some(tag_symbol) = tag_symbol {
                s.symbols = vec![tag_symbol];
            }
            s.completion_kind = CompletionKind::Global;
            s.keyword_filters = KeywordCompletionFilters::None;
        } else {
            // For JavaScript or TypeScript, if we're not after a dot, then just try to get the
            // global symbols in scope.  These results should be valid for either language as
            // the set of symbols that can be referenced from this location.
            let ok = s.try_get_global_symbols(self, ctx, type_checker)?;
            if !ok {
                if s.keyword_filters != KeywordCompletionFilters::None {
                    return Ok(Some(CompletionData::Keyword(keyword_completion_data(
                        s.keyword_filters,
                        is_js_only_location,
                        s.is_new_identifier_location,
                    ))));
                }
                return Ok(None);
            }
        }

        let mut contextual_type_or_constraint: Option<P<Type>> = None;
        if let Some(previous_token) = previous_token {
            contextual_type_or_constraint =
                get_contextual_type(previous_token, position, file, type_checker);
            if contextual_type_or_constraint.is_none() {
                contextual_type_or_constraint =
                    get_constraint_of_type_argument_property(Some(previous_token), type_checker);
            }
        }

        // exclude literal suggestions after <input type="text" [||] /> microsoft/TypeScript#51667) and after closing quote (microsoft/TypeScript#52675)
        // for strings getStringLiteralCompletions handles completions
        let is_literal_expected = previous_token
            .is_none_or(|token| !ast::is_string_literal_like(token))
            && !is_jsx_identifier_expected;
        let mut literals: Vec<LiteralValue> = Vec::new();
        if is_literal_expected {
            let mut types: Vec<P<Type>> = Vec::new();
            if let Some(t) = contextual_type_or_constraint {
                if t.is_union() {
                    types = t.types().to_vec();
                } else {
                    types = vec![t];
                }
            }
            literals = types
                .iter()
                .filter_map(|&t| {
                    if is_literal(t) && !t.is_enum_literal() {
                        t.as_literal_type().value()
                    } else {
                        None
                    }
                })
                .collect();
        }

        let mut recommended_completion: Option<P<Symbol>> = None;
        if let (Some(previous_token), Some(contextual_type_or_constraint)) =
            (previous_token, contextual_type_or_constraint)
        {
            recommended_completion = get_recommended_completion(
                previous_token,
                contextual_type_or_constraint,
                type_checker,
            );
        }

        if s.default_commit_characters.is_none() {
            s.default_commit_characters =
                Some(get_default_commit_characters(s.is_new_identifier_location));
        }

        Ok(Some(CompletionData::Data(Box::new(CompletionDataData {
            symbols: s.symbols,
            auto_imports: s.auto_imports,
            completion_kind: s.completion_kind,
            property_access_to_convert,
            is_new_identifier_location: s.is_new_identifier_location,
            location,
            keyword_filters: s.keyword_filters,
            literals,
            symbol_to_origin_info_map: s.symbol_to_origin_info_map,
            symbol_to_sort_text_map: s.symbol_to_sort_text_map,
            recommended_completion,
            previous_token,
            context_token: s.context_token,
            jsx_initializer,
            inside_jsdoc_tag_type_expression,
            is_type_only_location,
            is_jsx_identifier_expected,
            is_right_of_open_tag,
            is_right_of_dot_or_question_dot: is_right_of_dot || is_right_of_question_dot,
            import_statement_completion,
            has_unresolved_auto_imports: s.has_unresolved_auto_imports,
            default_commit_characters: s.default_commit_characters,
        }))))
    }
}

// The locals of Go's getCompletionData that its closures (completions.go:796-1704) read and mutate.
struct GetCompletionDataState {
    file: P<SourceFile>,
    position: u32,
    preferences: UserPreferences,
    for_item_resolve: bool,
    in_checked_file: bool,
    inside_jsdoc_tag_type_expression: bool,
    context_token: Option<P<Node>>,
    previous_token: Option<P<Node>>,
    node: P<Node>,
    location: P<Node>,
    is_right_of_dot: bool,
    is_right_of_question_dot: bool,
    is_right_of_open_tag: bool,
    import_statement_completion: Option<ImportStatementCompletionInfo>,
    is_type_only_location: bool,
    completion_kind: CompletionKind,
    has_unresolved_auto_imports: bool,
    symbols: Vec<P<Symbol>>,
    auto_imports: Vec<autoimport::FixAndExport>,
    symbol_to_origin_info_map: FxHashMap<usize, SymbolOriginInfo>,
    symbol_to_sort_text_map: FxHashMap<SymbolId, SortText>,
    seen_property_symbols: FxHashSet<SymbolId>,
    is_new_identifier_location: bool,
    default_commit_characters: Option<Vec<String>>,
    keyword_filters: KeywordCompletionFilters,
}

impl GetCompletionDataState {
    // completions.go:796
    fn add_symbol_origin_info(
        &mut self,
        symbol: P<Symbol>,
        insert_question_dot: bool,
        insert_await: bool,
    ) {
        let symbol_id = ast::get_symbol_id(symbol);
        if insert_await && self.seen_property_symbols.insert(symbol_id) {
            self.symbol_to_origin_info_map.insert(
                self.symbols.len() - 1,
                SymbolOriginInfo::with_kind(get_nullable_symbol_origin_info_kind(
                    symbolOriginInfoKind::Promise,
                    insert_question_dot,
                )),
            );
        } else if insert_question_dot {
            self.symbol_to_origin_info_map.insert(
                self.symbols.len() - 1,
                SymbolOriginInfo::with_kind(symbolOriginInfoKind::Nullable),
            );
        }
    }

    // completions.go:805
    fn add_symbol_sort_info(&mut self, symbol: P<Symbol>) {
        let symbol_id = ast::get_symbol_id(symbol);
        if is_static_property(symbol) {
            self.symbol_to_sort_text_map
                .insert(symbol_id, SORT_TEXT_LOCAL_DECLARATION_PRIORITY.to_string());
        }
    }

    // completions.go:812
    fn add_property_symbol(
        &mut self,
        type_checker: &mut Checker,
        symbol: P<Symbol>,
        insert_await: bool,
        insert_question_dot: bool,
    ) {
        // For a computed property with an accessible name like `Symbol.iterator`,
        // we'll add a completion for the *name* `Symbol` instead of for the property.
        // If this is e.g. [Symbol.iterator], add a completion for `Symbol`.
        let computed_property_name = symbol.declarations().iter().find_map(|&decl| {
            let name = ast::get_name_of_declaration(decl);
            match name {
                Some(name) if name.kind() == Kind::ComputedPropertyName => Some(name),
                _ => None,
            }
        });

        if let Some(computed_property_name) = computed_property_name {
            let left_most_name = get_left_most_name(computed_property_name.expression().unwrap()); // The completion is for `Symbol`, not `iterator`.
            let mut name_symbol: Option<P<Symbol>> = None;
            if let Some(left_most_name) = left_most_name {
                name_symbol = type_checker.get_symbol_at_location_exported(left_most_name);
            }
            // If this is nested like for `namespace N { export const sym = Symbol(); }`, we'll add the completion for `N`.
            let mut first_accessible_symbol: Option<P<Symbol>> = None;
            if let Some(name_symbol) = name_symbol {
                first_accessible_symbol =
                    get_first_symbol_in_chain(name_symbol, self.context_token, type_checker);
            }
            let mut first_accessible_symbol_id = SymbolId(0);
            if let Some(first_accessible_symbol) = first_accessible_symbol {
                first_accessible_symbol_id = ast::get_symbol_id(first_accessible_symbol);
            }
            if first_accessible_symbol_id != SymbolId(0)
                && self.seen_property_symbols.insert(first_accessible_symbol_id)
            {
                let first_accessible_symbol = first_accessible_symbol.unwrap();
                self.symbols.push(first_accessible_symbol);
                self.symbol_to_sort_text_map
                    .insert(first_accessible_symbol_id, SORT_TEXT_GLOBALS_OR_KEYWORDS.to_string());
                let module_symbol = first_accessible_symbol.parent();
                if module_symbol.is_none()
                    || !checker::is_external_module_symbol(module_symbol.unwrap())
                    || type_checker.try_get_member_in_module_exports_and_properties(
                        first_accessible_symbol.name(),
                        module_symbol.unwrap(),
                    ) != Some(first_accessible_symbol)
                {
                    self.symbol_to_origin_info_map.insert(
                        self.symbols.len() - 1,
                        SymbolOriginInfo::with_kind(get_nullable_symbol_origin_info_kind(
                            symbolOriginInfoKind::SymbolMember,
                            insert_question_dot,
                        )),
                    );
                } else {
                    // !!! auto-import symbol
                }
            } else if first_accessible_symbol_id == SymbolId(0)
                || !self.seen_property_symbols.contains(&first_accessible_symbol_id)
            {
                self.symbols.push(symbol);
                self.add_symbol_origin_info(symbol, insert_question_dot, insert_await);
                self.add_symbol_sort_info(symbol);
            }
        } else {
            self.symbols.push(symbol);
            self.add_symbol_origin_info(symbol, insert_question_dot, insert_await);
            self.add_symbol_sort_info(symbol);
        }
    }

    // completions.go:862
    fn add_type_properties(
        &mut self,
        type_checker: &mut Checker,
        t: P<Type>,
        insert_await: bool,
        insert_question_dot: bool,
    ) {
        if type_checker.get_string_index_type(t).is_some() {
            self.is_new_identifier_location = true;
            self.default_commit_characters = Some(Vec::new());
        }
        if self.is_right_of_question_dot && !type_checker.get_call_signatures(t).is_empty() {
            self.is_new_identifier_location = true;
            if self.default_commit_characters.is_none() {
                self.default_commit_characters = Some(strings(&ALL_COMMIT_CHARACTERS)); // Only invalid commit character here would be `(`.
            }
        }

        let property_access = if self.node.kind() == Kind::ImportType {
            self.node
        } else {
            self.node.parent().unwrap()
        };

        if self.in_checked_file {
            for symbol in type_checker.get_apparent_properties(t) {
                if type_checker.is_valid_property_access_for_completions_exported(
                    property_access,
                    t,
                    symbol,
                ) {
                    self.add_property_symbol(
                        type_checker,
                        symbol,
                        false, /*insertAwait*/
                        insert_question_dot,
                    );
                }
            }
        } else {
            // In javascript files, for union types, we don't just get the members that
            // the individual types have in common, we also include all the members that
            // each individual type has. This is because we're going to add all identifiers
            // anyways. So we might as well elevate the members that were at least part
            // of the individual types to a higher status since we know what they are.
            self.symbols.extend(get_properties_for_completion(t, type_checker).into_iter().filter(
                |&symbol| {
                    type_checker.is_valid_property_access_for_completions_exported(
                        property_access,
                        t,
                        symbol,
                    )
                },
            ));
        }

        if insert_await {
            let promise_type = type_checker.get_promised_type_of_promise(t);
            if let Some(promise_type) = promise_type {
                for symbol in type_checker.get_apparent_properties(promise_type) {
                    if type_checker.is_valid_property_access_for_completions_exported(
                        property_access,
                        promise_type,
                        symbol,
                    ) {
                        self.add_property_symbol(
                            type_checker,
                            symbol,
                            true, /*insertAwait*/
                            insert_question_dot,
                        );
                    }
                }
            }
        }
    }

    // completions.go:912
    fn get_type_script_member_symbols(&mut self, type_checker: &mut Checker) {
        // Right of dot member completion list
        self.completion_kind = CompletionKind::PropertyAccess;
        let node = self.node;
        let file = self.file;

        // Since this is qualified name check it's a type node location
        let is_import_type = ast::is_literal_import_type_node(node);
        let is_type_location = (is_import_type && !node.as_import_type_node().is_type_of)
            || ast::is_part_of_type_node(node.parent().unwrap())
            || is_possibly_type_argument_position(self.context_token, file, type_checker);
        let is_rhs_of_import_declaration =
            is_in_right_side_of_internal_import_equals_declaration(node);
        if ast::is_entity_name(node) || is_import_type || ast::is_property_access_expression(node) {
            let is_namespace_name = ast::is_module_declaration(node.parent().unwrap());
            if is_namespace_name {
                self.is_new_identifier_location = true;
                self.default_commit_characters = Some(Vec::new());
            }
            let symbol = type_checker.get_symbol_at_location_exported(node);
            if let Some(symbol) = symbol {
                let symbol = checker::skip_alias(symbol, type_checker);
                if symbol.flags().intersects(SymbolFlags::Module | SymbolFlags::Enum) {
                    let value_access_node =
                        if is_import_type { node } else { node.parent().unwrap() };
                    // Extract module or enum members
                    let exported_symbols = type_checker.get_exports_of_module_exported(symbol);
                    for exported_symbol in exported_symbols {
                        let is_valid_access = if is_namespace_name {
                            // At `namespace N.M/**/`, if this is the only declaration of `M`, don't include `M` as a completion.
                            exported_symbol.flags().intersects(SymbolFlags::Namespace)
                                && !exported_symbol
                                    .declarations()
                                    .iter()
                                    .all(|declaration| declaration.parent() == node.parent())
                        } else if is_rhs_of_import_declaration {
                            // Any kind is allowed when dotting off namespace in internal import equals declaration
                            symbol_can_be_referenced_at_type_location(
                                exported_symbol,
                                type_checker,
                                None,
                            ) || type_checker.is_valid_property_access_exported(
                                value_access_node,
                                exported_symbol.name(),
                            )
                        } else if is_type_location || self.inside_jsdoc_tag_type_expression {
                            symbol_can_be_referenced_at_type_location(
                                exported_symbol,
                                type_checker,
                                None,
                            )
                        } else {
                            type_checker.is_valid_property_access_exported(
                                value_access_node,
                                exported_symbol.name(),
                            )
                        };
                        if is_valid_access {
                            self.symbols.push(exported_symbol);
                        }
                    }

                    // If the module is merged with a value, we must get the type of the class and add its properties (for inherited static methods).
                    if !is_type_location
                        && !self.inside_jsdoc_tag_type_expression
                        && symbol.declarations().iter().any(|decl| {
                            decl.kind() != Kind::SourceFile
                                && decl.kind() != Kind::ModuleDeclaration
                                && decl.kind() != Kind::EnumDeclaration
                        })
                    {
                        let type_of_symbol = type_checker
                            .get_type_of_symbol_at_location(symbol, Some(node))
                            .unwrap();
                        let mut t = type_checker.get_non_optional_type(type_of_symbol);
                        let mut insert_question_dot = false;
                        if type_checker.is_nullable_type(t) {
                            let can_correct_to_question_dot = self.is_right_of_dot
                                && !self.is_right_of_question_dot
                                && !self
                                    .preferences
                                    .include_automatic_optional_chain_completions
                                    .is_false();
                            if can_correct_to_question_dot || self.is_right_of_question_dot {
                                t = type_checker.get_non_nullable_type(t);
                                if can_correct_to_question_dot {
                                    insert_question_dot = true;
                                }
                            }
                        }
                        self.add_type_properties(
                            type_checker,
                            t,
                            node.flags().intersects(NodeFlags::AwaitContext),
                            insert_question_dot,
                        );
                    }

                    return;
                }
            }
        }

        if !is_type_location || checker::is_in_type_query(node) {
            // microsoft/TypeScript#39946. Pulling on the type of a node inside of a function with a contextual `this` parameter can result in a circularity
            // if the `node` is part of the exprssion of a `yield` or `return`. This circularity doesn't exist at compile time because
            // we will check (and cache) the type of `this` *before* checking the type of the node.
            type_checker
                .try_get_this_type_at_ex_exported(node, false /*includeGlobalThis*/, None);
            let type_at_location = type_checker.get_type_at_location(node);
            let mut t = type_checker.get_non_optional_type(type_at_location);

            if is_type_location {
                let non_nullable = type_checker.get_non_nullable_type(t);
                self.add_type_properties(
                    type_checker,
                    non_nullable,
                    false, /*insertAwait*/
                    false, /*insertQuestionDot*/
                );
            } else {
                let mut insert_question_dot = false;
                if type_checker.is_nullable_type(t) {
                    let can_correct_to_question_dot = self.is_right_of_dot
                        && !self.is_right_of_question_dot
                        && !self
                            .preferences
                            .include_automatic_optional_chain_completions
                            .is_false();

                    if can_correct_to_question_dot || self.is_right_of_question_dot {
                        t = type_checker.get_non_nullable_type(t);
                        if can_correct_to_question_dot {
                            insert_question_dot = true;
                        }
                    }
                }
                self.add_type_properties(
                    type_checker,
                    t,
                    node.flags().intersects(NodeFlags::AwaitContext),
                    insert_question_dot,
                );
            }
        }
    }

    // Aggregates relevant symbols for completion in object literals in type argument positions.
    // completions.go:1026
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Completion branches share a fallible result with branches that resolve symbols or validate requests."
    )]
    fn try_get_object_type_literal_in_type_argument_completion_symbols(
        &mut self,
        type_checker: &mut Checker,
    ) -> Result<GlobalsSearch, lsproto::Error> {
        let Some(type_literal_node) = try_get_type_literal_node(self.context_token) else {
            return Ok(GlobalsSearch::Continue);
        };

        let intersection_type_node =
            if ast::is_intersection_type_node(type_literal_node.parent().unwrap()) {
                type_literal_node.parent()
            } else {
                None
            };
        let container_type_node = intersection_type_node.unwrap_or(type_literal_node);

        let Some(container_expected_type) =
            get_constraint_of_type_argument_property(Some(container_type_node), type_checker)
        else {
            return Ok(GlobalsSearch::Continue);
        };

        let container_actual_type =
            type_checker.get_type_from_type_node_exported(container_type_node);

        let members = get_properties_for_completion(container_expected_type, type_checker);
        let existing_members = get_properties_for_completion(container_actual_type, type_checker);

        let mut existing_member_names: FxHashSet<&'static str> = FxHashSet::default();
        existing_member_names.extend(existing_members.into_iter().map(|member| member.name()));

        self.symbols.extend(
            members.into_iter().filter(|member| !existing_member_names.contains(member.name())),
        );

        self.completion_kind = CompletionKind::ObjectPropertyDeclaration;
        self.is_new_identifier_location = true;

        Ok(GlobalsSearch::Success)
    }

    // Aggregates relevant symbols for completion in object literals and object binding patterns.
    // Relevant symbols are stored in the captured 'symbols' variable.
    // completions.go:1071
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Completion branches share a fallible result with branches that resolve symbols or validate requests."
    )]
    fn try_get_object_like_completion_symbols(
        &mut self,
        l: &LanguageService,
        ctx: &Context,
        type_checker: &mut Checker,
    ) -> Result<GlobalsSearch, lsproto::Error> {
        if let Some(context_token) = self.context_token
            && context_token.kind() == Kind::DotDotDotToken
        {
            return Ok(GlobalsSearch::Continue);
        }
        let file = self.file;
        let position = self.position;
        let Some(object_like_container) =
            try_get_object_like_completion_container(self.context_token, position, file)
        else {
            return Ok(GlobalsSearch::Continue);
        };

        // We're looking up possible property names from contextual/inferred/declared type.
        self.completion_kind = CompletionKind::ObjectPropertyDeclaration;

        let mut type_members: Vec<P<Symbol>> = Vec::new();
        let mut existing_members: &'static [P<Node>] = &[];

        if object_like_container.kind() == Kind::ObjectLiteralExpression {
            let instantiated_type =
                try_get_object_literal_contextual_type(object_like_container, type_checker);

            // Check completions for Object property value shorthand
            let Some(instantiated_type) = instantiated_type else {
                if object_like_container.flags().intersects(NodeFlags::InWithStatement) {
                    return Ok(GlobalsSearch::Fail);
                }
                return Ok(GlobalsSearch::Continue);
            };
            let completions_type = type_checker.get_contextual_type_exported(
                object_like_container,
                ContextFlags::IgnoreNodeInferences,
            );
            let t = completions_type.unwrap_or(instantiated_type);
            let string_index_type = type_checker.get_string_index_type(t);
            let number_index_type = type_checker.get_number_index_type(t);
            self.is_new_identifier_location =
                string_index_type.is_some() || number_index_type.is_some();
            type_members = get_properties_for_object_expression(
                instantiated_type,
                completions_type,
                object_like_container,
                type_checker,
            );
            existing_members = object_like_container.properties();

            if type_members.is_empty() {
                // Edge case: If NumberIndexType exists
                if number_index_type.is_none() {
                    return Ok(GlobalsSearch::Continue);
                }
            }
        } else {
            assert!(
                object_like_container.kind() == Kind::ObjectBindingPattern,
                "Expected 'objectLikeContainer' to be an object binding pattern."
            );
            // We are *only* completing on properties from the type being destructured.
            self.is_new_identifier_location = false;
            let root_declaration =
                ast::get_root_declaration(object_like_container.parent().unwrap());
            assert!(
                ast::is_variable_like(root_declaration),
                "Root declaration is not variable-like."
            );

            // We don't want to complete using the type acquired by the shape
            // of the binding pattern; we are only interested in types acquired
            // through type declaration or inference.
            // Also proceed if rootDeclaration is a parameter and if its containing function expression/arrow function is contextually typed -
            // type of parameter will flow in from the contextual type of the function.
            let mut can_get_type = ast::has_initializer(root_declaration)
                || ast::get_type_annotation_node(root_declaration).is_some()
                || root_declaration.parent().unwrap().parent().unwrap().kind()
                    == Kind::ForOfStatement;
            if !can_get_type && root_declaration.kind() == Kind::Parameter {
                let root_parent = root_declaration.parent().unwrap();
                if ast::is_expression(root_parent) {
                    can_get_type = type_checker
                        .get_contextual_type_exported(root_parent, ContextFlags::None)
                        .is_some();
                } else if root_parent.kind() == Kind::MethodDeclaration
                    || root_parent.kind() == Kind::SetAccessor
                {
                    can_get_type = ast::is_expression(root_parent.parent().unwrap())
                        && type_checker
                            .get_contextual_type_exported(
                                root_parent.parent().unwrap(),
                                ContextFlags::None,
                            )
                            .is_some();
                }
            }
            if can_get_type {
                // Go checks the type for nil; GetTypeAtLocation never returns nil.
                let type_for_object = type_checker.get_type_at_location(object_like_container);
                let properties = type_checker.get_properties_of_type_exported(type_for_object);
                type_members = properties
                    .iter()
                    .copied()
                    .filter(|&property_symbol| {
                        type_checker.is_property_accessible_exported(
                            object_like_container,
                            false, /*isSuper*/
                            false, /*isWrite*/
                            type_for_object,
                            property_symbol,
                        )
                    })
                    .collect();
                existing_members = object_like_container.elements();
            }
        }

        if !type_members.is_empty() {
            // Add filtered items to the completion list.
            let (filtered_members, spread_member_names) = filter_object_members_list(
                &type_members,
                existing_members,
                file,
                position,
                type_checker,
            );
            self.symbols.extend(filtered_members.iter().copied());

            // Set sort texts.
            for &member in &filtered_members {
                let symbol_id = ast::get_symbol_id(member);
                if spread_member_names.contains(member.name()) {
                    self.symbol_to_sort_text_map.insert(
                        symbol_id,
                        SORT_TEXT_MEMBER_DECLARED_BY_SPREAD_ASSIGNMENT.to_string(),
                    );
                }
                if member.flags().intersects(SymbolFlags::Optional) {
                    self.symbol_to_sort_text_map
                        .entry(symbol_id)
                        .or_insert_with(|| SORT_TEXT_OPTIONAL_MEMBER.to_string());
                }
                if object_like_container.kind() == Kind::ObjectLiteralExpression
                    && self
                        .preferences
                        .include_completions_with_object_literal_method_snippets
                        .is_true()
                {
                    let (display_name, _) = get_completion_entry_display_name_for_symbol(
                        file,
                        &self.preferences,
                        member,
                        None, /*origin*/
                        CompletionKind::ObjectPropertyDeclaration,
                        false, /*isJsxIdentifierExpected*/
                    );
                    if !display_name.is_empty() {
                        let sort_text = self.symbol_to_sort_text_map.entry(symbol_id).or_default();
                        let original_sort_text = if sort_text.is_empty() {
                            SORT_TEXT_LOCATION_PRIORITY
                        } else {
                            sort_text.as_str()
                        };
                        *sort_text =
                            object_literal_property_sort_text(original_sort_text, &display_name);
                    }
                }
            }

            if object_like_container.kind() == Kind::ObjectLiteralExpression
                && self
                    .preferences
                    .include_completions_with_object_literal_method_snippets
                    .is_true()
            {
                for entry in l.collect_object_literal_method_symbols(
                    ctx,
                    type_checker,
                    &filtered_members,
                    object_like_container,
                    file,
                ) {
                    self.symbol_to_origin_info_map.insert(self.symbols.len(), entry.origin);
                    self.symbols.push(entry.symbol);
                }
            }
        }

        Ok(GlobalsSearch::Success)
    }

    // completions.go:1202
    fn should_offer_import_completions(&self) -> bool {
        if tspath::is_dynamic_file_name(self.file.file_name()) {
            return false;
        }
        // If already typing an import statement, provide completions for it.
        if self.import_statement_completion.is_some() {
            return true;
        }
        // If not already a module, must have modules enabled.
        if self.preferences.include_completions_for_module_exports.is_false() {
            return false;
        }
        // Always using ES modules in 6.0+
        true
    }

    // Mutates `symbols`, `symbolToOriginInfoMap`, and `symbolToSortTextMap`
    // completions.go:1219
    fn collect_auto_imports(
        &mut self,
        l: &LanguageService,
        type_checker: &mut Checker,
    ) -> Result<(), lsproto::Error> {
        // `completionItem/resolve` for auto-import completions should be resolved via the completion item data,
        // so we don't need to collect auto-import entries again.
        if self.for_item_resolve {
            return Ok(());
        }
        if !self.should_offer_import_completions() {
            return Ok(());
        }

        // import { type | -> token text should be blank
        let mut lower_case_token_text = String::new();
        let (mut usage_position, mut fidelity) = l.create_lsp_position(self.position, self.file);
        if !fidelity.is_exact() {
            return Ok(());
        }
        if let Some(previous_token) = self.previous_token
            && ast::is_identifier(previous_token)
        {
            (usage_position, fidelity) = l.create_lsp_position(
                scanner::get_token_pos_of_node(
                    previous_token,
                    self.file,
                    false, /*includeJSDoc*/
                ),
                self.file,
            );
            if !fidelity.is_exact() {
                return Ok(());
            }
            if !(Some(previous_token) == self.context_token
                && self.import_statement_completion.is_some())
            {
                lower_case_token_text = previous_token.text().to_lowercase();
            }
        }

        let view = l.get_prepared_auto_import_view(self.file, type_checker)?;
        // Go checks the view for nil; a prepared view is never nil.

        self.auto_imports = view.get_completions(
            &lower_case_token_text,
            usage_position,
            self.is_right_of_open_tag,
            self.is_type_only_location,
        );
        Ok(())
    }

    // completions.go:1257
    fn try_get_import_completion_symbols(
        &mut self,
        l: &LanguageService,
        type_checker: &mut Checker,
    ) -> Result<GlobalsSearch, lsproto::Error> {
        if self.import_statement_completion.is_none() {
            return Ok(GlobalsSearch::Continue);
        }
        self.is_new_identifier_location = true;
        self.collect_auto_imports(l, type_checker)?;
        Ok(GlobalsSearch::Success)
    }

    // Aggregates relevant symbols for completion in import clauses and export clauses
    // whose declarations have a module specifier; for instance, symbols will be aggregated for
    //
    //      import { | } from "moduleName";
    //      export { a as foo, | } from "moduleName";
    //
    // but not for
    //
    //      export { | };
    //
    // Relevant symbols are stored in the captured 'symbols' variable.
    // completions.go:1279
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Completion branches share a fallible result with branches that resolve symbols or validate requests."
    )]
    fn try_get_import_or_export_clause_completion_symbols(
        &mut self,
        type_checker: &mut Checker,
    ) -> Result<GlobalsSearch, lsproto::Error> {
        let Some(context_token) = self.context_token else {
            return Ok(GlobalsSearch::Continue);
        };

        // `import { |` or `import { a as 0, | }` or `import { type | }`
        let mut named_imports_or_exports: Option<P<Node>> = None;
        if context_token.kind() == Kind::OpenBraceToken || context_token.kind() == Kind::CommaToken
        {
            named_imports_or_exports =
                if is_named_imports_or_exports(context_token.parent().unwrap()) {
                    context_token.parent()
                } else {
                    None
                };
        } else if is_type_keyword_token_or_identifier(context_token) {
            let grand_parent = context_token.parent().unwrap().parent().unwrap();
            named_imports_or_exports =
                if is_named_imports_or_exports(grand_parent) { Some(grand_parent) } else { None };
        }

        let Some(named_imports_or_exports) = named_imports_or_exports else {
            return Ok(GlobalsSearch::Continue);
        };

        // We can at least offer `type` at `import { |`
        if !is_type_keyword_token_or_identifier(context_token) {
            self.keyword_filters = KeywordCompletionFilters::TypeKeyword;
        }

        // try to show exported member for imported/re-exported module
        let module_specifier = if named_imports_or_exports.kind() == Kind::NamedImports {
            named_imports_or_exports.parent().unwrap().parent().unwrap()
        } else {
            named_imports_or_exports.parent().unwrap()
        }
        .module_specifier();
        let Some(module_specifier) = module_specifier else {
            self.is_new_identifier_location = true;
            if named_imports_or_exports.kind() == Kind::NamedImports {
                return Ok(GlobalsSearch::Fail);
            }
            return Ok(GlobalsSearch::Continue);
        };

        let Some(module_specifier_symbol) =
            type_checker.get_symbol_at_location_exported(module_specifier)
        else {
            self.is_new_identifier_location = true;
            return Ok(GlobalsSearch::Fail);
        };

        self.completion_kind = CompletionKind::MemberLike;
        self.is_new_identifier_location = false;
        let exports = type_checker.get_exports_and_properties_of_module(module_specifier_symbol);

        let mut existing: FxHashSet<&'static str> = FxHashSet::default();
        existing.extend(named_imports_or_exports.elements().iter().filter_map(|&element| {
            if is_currently_editing_node(element, self.file, self.position) {
                return None;
            }
            Some(element.property_name_or_name().unwrap().text())
        }));
        let uniques: Vec<P<Symbol>> = exports
            .into_iter()
            .filter(|&symbol| {
                ast::symbol_name(symbol) != ast::INTERNAL_SYMBOL_NAME_DEFAULT
                    && !existing.contains(ast::symbol_name(symbol))
            })
            .collect();

        let uniques_is_empty = uniques.is_empty();
        self.symbols.extend(uniques);
        if uniques_is_empty {
            // If there's nothing else to import, don't offer `type` either.
            self.keyword_filters = KeywordCompletionFilters::None;
        }
        Ok(GlobalsSearch::Success)
    }

    // import { x } from "foo" with { | }
    // completions.go:1349
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Completion branches share a fallible result with branches that resolve symbols or validate requests."
    )]
    fn try_get_import_attributes_completion_symbols(
        &mut self,
        type_checker: &mut Checker,
    ) -> Result<GlobalsSearch, lsproto::Error> {
        let Some(context_token) = self.context_token else {
            return Ok(GlobalsSearch::Continue);
        };

        let import_attributes = match context_token.kind() {
            Kind::OpenBraceToken | Kind::CommaToken => context_token.parent(),
            Kind::ColonToken => context_token.parent().unwrap().parent(),
            _ => None,
        };
        let Some(import_attributes) = import_attributes.filter(|&n| ast::is_import_attributes(n))
        else {
            return Ok(GlobalsSearch::Continue);
        };

        // Go checks the attribute list for nil; the parser always creates it.
        let elements: &'static [P<Node>] =
            import_attributes.as_import_attributes().attributes.nodes();
        let existing: FxHashSet<&'static str> =
            elements.iter().map(|el| el.as_import_attribute().name.unwrap().text()).collect();
        let type_at_location = type_checker.get_type_at_location(import_attributes);
        let uniques: Vec<P<Symbol>> = type_checker
            .get_apparent_properties(type_at_location)
            .into_iter()
            .filter(|&symbol| !existing.contains(ast::symbol_name(symbol)))
            .collect();
        self.symbols.extend(uniques);
        Ok(GlobalsSearch::Success)
    }

    // Adds local declarations for completions in named exports:
    //   export { | };
    // Does not check for the absence of a module specifier (`export {} from "./other"`)
    // because `tryGetImportOrExportClauseCompletionSymbols` runs first and handles that,
    // preventing this function from running.
    // completions.go:1388
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Completion branches share a fallible result with branches that resolve symbols or validate requests."
    )]
    fn try_get_local_named_export_completion_symbols(
        &mut self,
    ) -> Result<GlobalsSearch, lsproto::Error> {
        let Some(context_token) = self.context_token else {
            return Ok(GlobalsSearch::Continue);
        };
        let mut named_exports: Option<P<Node>> = None;
        if context_token.kind() == Kind::OpenBraceToken || context_token.kind() == Kind::CommaToken
        {
            named_exports = if ast::is_named_exports(context_token.parent().unwrap()) {
                context_token.parent()
            } else {
                None
            };
        }

        let Some(named_exports) = named_exports else {
            return Ok(GlobalsSearch::Continue);
        };

        let locals_container = ast::find_ancestor(named_exports, |node| {
            ast::is_source_file(node) || ast::is_module_declaration(node)
        })
        .unwrap();
        self.completion_kind = CompletionKind::None;
        self.is_new_identifier_location = false;
        let local_symbol = locals_container.symbol();
        let mut local_exports: Option<P<ast::SymbolTable>> = None;
        if let Some(local_symbol) = local_symbol {
            local_exports = local_symbol.exports();
        }
        if let Some(locals) = locals_container.locals() {
            for (name, symbol) in locals.entries() {
                self.symbols.push(symbol);
                if local_exports.is_some_and(|e| e.has(name)) {
                    let symbol_id = ast::get_symbol_id(symbol);
                    self.symbol_to_sort_text_map
                        .insert(symbol_id, SORT_TEXT_OPTIONAL_MEMBER.to_string());
                }
            }
        }

        Ok(GlobalsSearch::Success)
    }

    // completions.go:1422
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Completion branches share a fallible result with branches that resolve symbols or validate requests."
    )]
    fn try_get_constructor_completion(&mut self) -> Result<GlobalsSearch, lsproto::Error> {
        if try_get_constructor_like_completion_container(self.context_token).is_none() {
            return Ok(GlobalsSearch::Continue);
        }

        // no members, only keywords
        self.completion_kind = CompletionKind::None;
        // Declaring new property/method/accessor
        self.is_new_identifier_location = true;
        // Has keywords for constructor parameter
        self.keyword_filters = KeywordCompletionFilters::ConstructorParameterKeywords;
        Ok(GlobalsSearch::Success)
    }

    // Aggregates relevant symbols for completion in class declaration
    // Relevant symbols are stored in the captured 'symbols' variable.
    // completions.go:1438
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Completion branches share a fallible result with branches that resolve symbols or validate requests."
    )]
    fn try_get_class_like_completion_symbols(
        &mut self,
        type_checker: &mut Checker,
    ) -> Result<GlobalsSearch, lsproto::Error> {
        let file = self.file;
        let position = self.position;
        let Some(decl) = try_get_object_type_declaration_completion_container(
            file,
            self.context_token,
            self.location,
            position,
        ) else {
            return Ok(GlobalsSearch::Continue);
        };
        let context_token = self.context_token.unwrap();

        // We're looking up possible property names from parent type.
        self.completion_kind = CompletionKind::MemberLike;
        // Declaring new property/method/accessor
        self.is_new_identifier_location = true;
        if context_token.kind() == Kind::AsteriskToken {
            self.keyword_filters = KeywordCompletionFilters::None;
        } else if ast::is_class_like(decl) {
            self.keyword_filters = KeywordCompletionFilters::ClassElementKeywords;
        } else {
            self.keyword_filters = KeywordCompletionFilters::InterfaceElementKeywords;
        }

        // If you're in an interface you don't want to repeat things from super-interface. So just stop here.
        if !ast::is_class_like(decl) {
            return Ok(GlobalsSearch::Success);
        }

        let class_element = if context_token.kind() == Kind::SemicolonToken {
            context_token.parent().unwrap().parent().unwrap()
        } else {
            context_token.parent().unwrap()
        };
        let mut class_element_modifier_flags = ModifierFlags::None;
        if ast::is_class_element(class_element) {
            class_element_modifier_flags = class_element.modifier_flags();
        }
        // If this is context token is not something we are editing now, consider if this would lead to be modifier.
        if context_token.kind() == Kind::Identifier
            && !is_currently_editing_node(context_token, file, position)
        {
            match context_token.text() {
                "private" => class_element_modifier_flags |= ModifierFlags::Private,
                "static" => class_element_modifier_flags |= ModifierFlags::Static,
                "override" => class_element_modifier_flags |= ModifierFlags::Override,
                _ => {}
            }
        }
        if ast::is_class_static_block_declaration(class_element) {
            class_element_modifier_flags |= ModifierFlags::Static;
        }

        // No member list for private methods
        if !class_element_modifier_flags.intersects(ModifierFlags::Private) {
            // List of property symbols of base type that are not private and already implemented
            let base_type_nodes: Vec<P<Node>> = if ast::is_class_like(decl)
                && class_element_modifier_flags.intersects(ModifierFlags::Override)
            {
                ast::get_class_extends_heritage_element(decl).into_iter().collect()
            } else {
                crate::utilities::get_all_super_type_nodes(decl)
            };
            let mut base_symbols: Vec<P<Symbol>> = Vec::new();
            base_symbols.extend(
                base_type_nodes
                    .into_iter()
                    .filter_map(|base_type_node| {
                        let t = type_checker.get_type_at_location(base_type_node);
                        if class_element_modifier_flags.intersects(ModifierFlags::Static) {
                            if let Some(t_symbol) = t.symbol() {
                                let type_of_symbol = type_checker
                                    .get_type_of_symbol_at_location(t_symbol, Some(decl))
                                    .unwrap();
                                Some(
                                    type_checker
                                        .get_properties_of_type_exported(type_of_symbol)
                                        .iter()
                                        .copied(),
                                )
                            } else {
                                None
                            }
                        } else {
                            Some(type_checker.get_properties_of_type_exported(t).iter().copied())
                        }
                    })
                    .flatten(),
            );

            self.symbols.extend(filter_class_members_list(
                &base_symbols,
                decl.members(),
                class_element_modifier_flags,
                file,
                position,
            ));
            for (index, element) in self.symbols.iter().enumerate() {
                let symbol = *element;
                let declaration = symbol.value_declaration();
                if let Some(declaration) = declaration
                    && ast::is_class_element(declaration)
                    && declaration.name().is_some()
                    && ast::is_computed_property_name(declaration.name().unwrap())
                {
                    let origin = SymbolOriginInfo {
                        kind: symbolOriginInfoKind::ComputedPropertyName,
                        data: SymbolOriginData::ComputedPropertyName(
                            SymbolOriginInfoComputedPropertyName {
                                symbol_name: type_checker.symbol_to_string_exported(symbol),
                            },
                        ),
                    };
                    self.symbol_to_origin_info_map.insert(index, origin);
                }
            }
        }

        Ok(GlobalsSearch::Success)
    }

    // completions.go:1529
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Completion branches share a fallible result with branches that resolve symbols or validate requests."
    )]
    fn try_get_jsx_completion_symbols(
        &mut self,
        type_checker: &mut Checker,
    ) -> Result<GlobalsSearch, lsproto::Error> {
        let Some(jsx_container) = try_get_containing_jsx_element(self.context_token, self.file)
        else {
            return Ok(GlobalsSearch::Continue);
        };
        // Cursor is inside a JSX self-closing element or opening element.
        let attributes = jsx_container.attributes().unwrap();
        let Some(attrs_type) =
            type_checker.get_contextual_type_exported(attributes, ContextFlags::None)
        else {
            return Ok(GlobalsSearch::Continue);
        };
        let completions_type = type_checker
            .get_contextual_type_exported(attributes, ContextFlags::IgnoreNodeInferences);
        let properties = get_properties_for_object_expression(
            attrs_type,
            completions_type,
            attributes,
            type_checker,
        );
        let (filtered_symbols, spread_member_names) = filter_jsx_attributes(
            &properties,
            attributes.properties(),
            self.file,
            self.position,
            type_checker,
        );

        self.symbols.extend(filtered_symbols.iter().copied());
        // Set sort texts.
        for &symbol in &filtered_symbols {
            let symbol_id = ast::get_symbol_id(symbol);
            if spread_member_names.contains(ast::symbol_name(symbol)) {
                self.symbol_to_sort_text_map
                    .insert(symbol_id, SORT_TEXT_MEMBER_DECLARED_BY_SPREAD_ASSIGNMENT.to_string());
            }
            if symbol.flags().intersects(SymbolFlags::Optional) {
                self.symbol_to_sort_text_map
                    .entry(symbol_id)
                    .or_insert_with(|| SORT_TEXT_OPTIONAL_MEMBER.to_string());
            }
        }

        self.completion_kind = CompletionKind::MemberLike;
        self.is_new_identifier_location = false;
        Ok(GlobalsSearch::Success)
    }

    // completions.go:1568
    fn get_global_completions(
        &mut self,
        l: &LanguageService,
        type_checker: &mut Checker,
    ) -> Result<GlobalsSearch, lsproto::Error> {
        let file = self.file;
        let position = self.position;
        let context_token = self.context_token;
        let previous_token = self.previous_token;
        if try_get_function_like_body_completion_container(context_token).is_some() {
            self.keyword_filters = KeywordCompletionFilters::FunctionLikeBodyKeywords;
        } else {
            self.keyword_filters = KeywordCompletionFilters::All;
        }
        // Get all entities in the current scope.
        self.completion_kind = CompletionKind::Global;
        let (is_new_identifier_location, default_commit_characters) =
            compute_commit_characters_and_is_new_identifier(context_token, file, position);
        self.is_new_identifier_location = is_new_identifier_location;
        self.default_commit_characters = Some(default_commit_characters);

        if previous_token != context_token {
            assert!(
                previous_token.is_some(),
                "Expected 'contextToken' to be defined when different from 'previousToken'."
            );
        }

        // We need to find the node that will give us an appropriate scope to begin
        // aggregating completion candidates. This is achieved in 'getScopeNode'
        // by finding the first node that encompasses a position, accounting for whether a node
        // is "complete" to decide whether a position belongs to the node.
        //
        // However, at the end of an identifier, we are interested in the scope of the identifier
        // itself, but fall outside of the identifier. For instance:
        //
        //      xyz => x$
        //
        // the cursor is outside of both the 'x' and the arrow function 'xyz => x',
        // so 'xyz' is not returned in our results.
        //
        // We define 'adjustedPosition' so that we may appropriately account for
        // being at the end of an identifier. The intention is that if requesting completion
        // at the end of an identifier, it should be effectively equivalent to requesting completion
        // anywhere inside/at the beginning of the identifier. So in the previous case, the
        // 'adjustedPosition' will work as if requesting completion in the following:
        //
        //      xyz => $x
        //
        // If previousToken !== contextToken, then
        //   - 'contextToken' was adjusted to the token prior to 'previousToken'
        //      because we were at the end of an identifier.
        //   - 'previousToken' is defined.
        let adjusted_position = if previous_token == context_token {
            position
        } else {
            astnav::get_start_of_node(previous_token.unwrap(), file, false /*includeJSDoc*/)
        };

        let scope_node =
            get_scope_node(context_token, adjusted_position, file).unwrap_or(file.as_node());

        let symbol_meanings =
            (if self.is_type_only_location { SymbolFlags::None } else { SymbolFlags::Value })
                | SymbolFlags::Type
                | SymbolFlags::Namespace
                | SymbolFlags::Alias;
        let type_only_alias_needs_promotion = previous_token.is_some()
            && !ast::is_valid_type_only_alias_use_site(previous_token.unwrap());

        self.symbols
            .extend(type_checker.get_symbols_in_scope_exported(scope_node, symbol_meanings));
        check_each_defined(&self.symbols, "getSymbolsInScope() should all be defined");
        for (index, element) in self.symbols.iter().enumerate() {
            let symbol = *element;
            let symbol_id = ast::get_symbol_id(symbol);
            if !type_checker.is_arguments_symbol(symbol)
                && !symbol
                    .declarations()
                    .iter()
                    .any(|&decl| ast::get_source_file_of_node(decl) == Some(file))
            {
                self.symbol_to_sort_text_map
                    .insert(symbol_id, SORT_TEXT_GLOBALS_OR_KEYWORDS.to_string());
            }
            if type_only_alias_needs_promotion && !symbol.flags().intersects(SymbolFlags::Value) {
                let type_only_alias_declaration = symbol
                    .declarations()
                    .iter()
                    .copied()
                    .find(|&d| ast::is_type_only_import_declaration(d));
                if let Some(type_only_alias_declaration) = type_only_alias_declaration {
                    let origin = SymbolOriginInfo {
                        kind: symbolOriginInfoKind::TypeOnlyAlias,
                        data: SymbolOriginData::TypeOnlyAlias(SymbolOriginInfoTypeOnlyAlias {
                            declaration: type_only_alias_declaration,
                        }),
                    };
                    self.symbol_to_origin_info_map.insert(index, origin);
                }
            }
        }

        // Need to insert 'this.' before properties of `this` type.
        if scope_node.kind() != Kind::SourceFile {
            let this_type = type_checker.try_get_this_type_at_ex_exported(
                scope_node,
                false, /*includeGlobalThis*/
                if ast::is_class_like(scope_node.parent().unwrap()) {
                    Some(scope_node)
                } else {
                    None
                },
            );
            if let Some(this_type) = this_type
                && !is_probably_global_type(this_type, file, type_checker)
            {
                for symbol in get_properties_for_completion(this_type, type_checker) {
                    let symbol_id = ast::get_symbol_id(symbol);
                    self.symbols.push(symbol);
                    self.symbol_to_origin_info_map.insert(
                        self.symbols.len() - 1,
                        SymbolOriginInfo::with_kind(symbolOriginInfoKind::ThisType),
                    );
                    self.symbol_to_sort_text_map
                        .insert(symbol_id, SORT_TEXT_SUGGESTED_CLASS_MEMBERS.to_string());
                }
            }
        }

        self.collect_auto_imports(l, type_checker)?;
        if self.is_type_only_location {
            if context_token.is_some()
                && ast::is_assertion_expression(context_token.unwrap().parent().unwrap())
            {
                self.keyword_filters = KeywordCompletionFilters::TypeAssertionKeywords;
            } else {
                self.keyword_filters = KeywordCompletionFilters::TypeKeywords;
            }
        }

        Ok(GlobalsSearch::Success)
    }

    // completions.go:1679
    fn try_get_global_symbols(
        &mut self,
        l: &LanguageService,
        ctx: &Context,
        type_checker: &mut Checker,
    ) -> Result<bool, lsproto::Error> {
        let mut result = GlobalsSearch::Continue;
        for index in 0..10 {
            result = match index {
                0 => self.try_get_object_type_literal_in_type_argument_completion_symbols(
                    type_checker,
                )?,
                1 => self.try_get_object_like_completion_symbols(l, ctx, type_checker)?,
                2 => self.try_get_import_completion_symbols(l, type_checker)?,
                3 => self.try_get_import_or_export_clause_completion_symbols(type_checker)?,
                4 => self.try_get_import_attributes_completion_symbols(type_checker)?,
                5 => self.try_get_local_named_export_completion_symbols()?,
                6 => self.try_get_constructor_completion()?,
                7 => self.try_get_class_like_completion_symbols(type_checker)?,
                8 => self.try_get_jsx_completion_symbols(type_checker)?,
                _ => self.get_global_completions(l, type_checker)?,
            };
            if result != GlobalsSearch::Continue {
                break;
            }
        }
        Ok(result == GlobalsSearch::Success)
    }
}

// Go `core.CheckEachDefined`: the slices hold non-nil pointers by construction, so there is nothing to check.
fn check_each_defined<'a, T>(s: &'a [T], _msg: &str) -> &'a [T] {
    s
}

// completions.go:1802
fn keyword_completion_data(
    keyword_filters: KeywordCompletionFilters,
    filter_out_ts_only_keywords: bool,
    is_new_identifier_location: bool,
) -> CompletionDataKeyword {
    CompletionDataKeyword {
        keyword_completions: get_keyword_completions(keyword_filters, filter_out_ts_only_keywords),
        is_new_identifier_location,
    }
}

// completions.go:1813
pub(crate) fn get_default_commit_characters(is_new_identifier_location: bool) -> Vec<String> {
    if is_new_identifier_location {
        return Vec::new();
    }
    strings(&ALL_COMMIT_CHARACTERS)
}
