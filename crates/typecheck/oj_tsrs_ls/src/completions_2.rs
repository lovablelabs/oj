// completions.go, lines 1820-3466.

use std::cell::RefCell;
use std::fmt::Write as _;
use std::rc::Rc;
use std::sync::Arc;

use rustc_hash::FxHashSet;
use tsrs_ast::{
    self as ast, Kind, ModifierFlags, Node, NodeFactory, SourceFile, Symbol, SymbolFlags, SymbolId,
};
use tsrs_checker::{
    self as checker, Checker, ContextFlags, Flags, LiteralValue, MemberOverrideStatus,
    SignatureKind, Type, TypeFlags, UnionReduction,
};
use tsrs_core::context::{Context, locale_from_context};
use tsrs_core::{CompilerOptions, LanguageVariant, P, Tristate, json, tspath};
use tsrs_lsproto as lsproto;
use tsrs_printer::{self as printer, EmitContext, SnippetElement, SnippetKind};
use tsrs_scanner as scanner;

use crate::astnav;
use crate::autoimport::{self, ImportAdder};
use crate::change;
use crate::codeactions_missingmemberfixer::{new_missing_member_fixer, preserveOptionalFlags};
use crate::completions::*;
use crate::languageservice::LanguageService;
use crate::lsutil::{self, JsxAttributeCompletionStyle, QuotePreference, UserPreferences};
use crate::utilities::{
    TrackerValue, get_possible_generic_signatures, get_possible_type_arguments_info,
    new_case_clause_tracker, quote,
};

// Go passes `literalValue` (string | jsnum.Number | PseudoBigInt) to `caseClauseTracker.hasValue`.
pub(crate) fn tracker_value(literal: &LiteralValue) -> TrackerValue {
    match literal {
        LiteralValue::String(s) => TrackerValue::String(s.to_string()),
        LiteralValue::Number(n) => TrackerValue::Number(*n),
        LiteralValue::BigInt(b) => TrackerValue::BigInt(*b),
        LiteralValue::Boolean(_) => panic!("Unsupported type: bool"),
    }
}

impl LanguageService {
    // completions.go:1820
    pub(crate) fn completion_info_from_data(
        &self,
        ctx: &Context,
        type_checker: &mut Checker,
        file: P<SourceFile>,
        compiler_options: P<CompilerOptions>,
        data: &mut CompletionDataData,
        position: u32,
        optional_replacement_span: Option<lsproto::Range>,
        include_symbols: bool,
    ) -> Result<Option<CompletionList>, lsproto::Error> {
        let keyword_filters = data.keyword_filters;
        let is_new_identifier_location = data.is_new_identifier_location;
        let context_token = data.context_token;
        let mut literals = data.literals.clone();
        let preferences = self.user_preferences().clone();

        // Verify if the file is JSX language variant
        if file.language_variant() == LanguageVariant::JSX {
            let list = self.get_jsx_closing_tag_completion(ctx, data.location, file, position);
            if list.is_some() {
                return Ok(list);
            }
        }

        // When the completion is for the expression of a case clause (e.g. `case |`),
        // filter literals & enum symbols whose values are already present in existing case clauses.
        let case_clause = ast::find_ancestor(context_token, ast::is_case_clause);
        if let Some(case_clause) = case_clause {
            let context_token = context_token.unwrap();
            if context_token.kind() == Kind::CaseKeyword
                || ast::is_node_descendant_of(context_token, case_clause.expression())
            {
                let tracker = new_case_clause_tracker(
                    type_checker,
                    case_clause.parent().unwrap().as_case_block().clauses.nodes(),
                );
                literals.retain(|literal| !tracker.has_value(&tracker_value(literal)));
                data.symbols.retain(|&symbol| {
                    if let Some(value_declaration) = symbol.value_declaration()
                        && ast::is_enum_member(value_declaration)
                    {
                        let value = type_checker.get_constant_value(value_declaration);
                        if let Some(value) = value
                            && tracker.has_value(&tracker_value(&value))
                        {
                            return false;
                        }
                    }
                    true
                });
            }
        }

        let is_checked = is_checked_file(file, &compiler_options);
        if is_checked
            && !is_new_identifier_location
            && data.symbols.is_empty()
            && keyword_filters == KeywordCompletionFilters::None
        {
            return Ok(None);
        }

        let (mut unique_names, mut sorted_entries) = self.get_completion_entries_from_symbols(
            ctx,
            type_checker,
            data,
            None, /*replacementToken*/
            position,
            file,
            compiler_options,
            include_symbols,
        )?;

        if data.keyword_filters != KeywordCompletionFilters::None {
            let keyword_completions = get_keyword_completions(
                data.keyword_filters,
                !data.inside_jsdoc_tag_type_expression && ast::is_source_file_js(file),
            );
            sorted_entries.extend(keyword_completions.into_iter().filter_map(|keyword_entry| {
                if data.is_type_only_location
                    && crate::utilities::is_type_keyword(scanner::string_to_token(
                        &keyword_entry.label,
                    ))
                    || !data.is_type_only_location
                        && is_contextual_keyword_in_auto_importable_expression_space(
                            &keyword_entry.label,
                        )
                    || !unique_names.contains(&keyword_entry.label)
                {
                    unique_names.insert(keyword_entry.label.clone());
                    Some(keyword_entry)
                } else {
                    None
                }
            }));
        }

        sorted_entries.extend(
            get_contextual_keywords(file, context_token, position).into_iter().filter_map(
                |keyword_entry| {
                    if unique_names.contains(&keyword_entry.label) {
                        None
                    } else {
                        unique_names.insert(keyword_entry.label.clone());
                        Some(CompletionItem::new(keyword_entry))
                    }
                },
            ),
        );

        sorted_entries.extend(literals.iter().map(|literal| {
            let literal_entry = create_completion_item_for_literal(file, &preferences, literal);
            unique_names.insert(literal_entry.label.clone());
            CompletionItem::new(literal_entry)
        }));

        if !is_checked {
            sorted_entries = Self::get_js_completion_entries(
                ctx,
                file,
                position,
                &mut unique_names,
                sorted_entries,
            );
        }

        if let Some(context_token) = context_token
            && !data.is_right_of_open_tag
            && !data.is_right_of_dot_or_question_dot
            && let Some(case_block) = ast::find_ancestor_kind(context_token, Kind::CaseBlock)
        {
            let cases_item = self.get_exhaustive_case_snippets(
                ctx,
                case_block,
                file,
                position,
                compiler_options,
                self.get_program(),
                type_checker,
            )?;
            if let Some(cases_item) = cases_item {
                sorted_entries.push(CompletionItem::new(cases_item));
            }
        }

        let item_defaults = self.set_item_defaults(
            ctx,
            position,
            file,
            &mut sorted_entries,
            Some(data.default_commit_characters.as_deref().unwrap_or(&[])),
            optional_replacement_span,
        );

        Ok(Some(CompletionList {
            is_incomplete: data.has_unresolved_auto_imports,
            item_defaults,
            items: sorted_entries,
            ..Default::default()
        }))
    }

    // completions.go:1955
    pub(crate) fn get_completion_entries_from_symbols(
        &self,
        ctx: &Context,
        type_checker: &mut Checker,
        data: &CompletionDataData,
        replacement_token: Option<P<Node>>,
        position: u32,
        file: P<SourceFile>,
        compiler_options: P<CompilerOptions>,
        include_symbols: bool,
    ) -> Result<(FxHashSet<String>, Vec<CompletionItem>), lsproto::Error> {
        let closest_symbol_declaration =
            get_closest_symbol_declaration(data.context_token, data.location);
        let use_semicolons = lsutil::probably_uses_semicolons(file);
        let preferences = self.user_preferences().clone();
        let is_member_completion = is_member_completion_kind(data.completion_kind);
        let mut sorted_entries: Vec<CompletionItem> =
            Vec::with_capacity(data.symbols.len() + data.auto_imports.len());
        // Tracks unique names.
        // Value is set to false for global variables or completions from external module exports, because we can have multiple of those;
        // true otherwise. Based on the order we add things we will always see locals first, then globals, then module exports.
        // So adding a completion for a local will prevent us from adding completions for external module exports sharing the same name.
        let mut uniques = UniqueNamesMap::default();
        for (index, &symbol) in data.symbols.iter().enumerate() {
            let origin = data.symbol_to_origin_info_map.get(&index);
            let (name, needs_convert_property_access) =
                get_completion_entry_display_name_for_symbol(
                    file,
                    &preferences,
                    symbol,
                    origin,
                    data.completion_kind,
                    data.is_jsx_identifier_expected,
                );
            if name.is_empty()
                || uniques.get(&name).copied().unwrap_or(false)
                    && (origin.is_none() || !origin_is_object_literal_method(origin))
                || data.completion_kind == CompletionKind::Global
                    && !should_include_symbol(
                        symbol,
                        data,
                        closest_symbol_declaration,
                        file,
                        type_checker,
                        &compiler_options,
                    )
            {
                continue;
            }

            // When in a value location in a JS file, ignore symbols that definitely seem to be type-only.
            if !data.is_type_only_location
                && ast::is_source_file_js(file)
                && symbol_appears_to_be_type_only(symbol, type_checker)
            {
                continue;
            }

            let mut original_sort_text = data
                .symbol_to_sort_text_map
                .get(&ast::get_symbol_id(symbol))
                .cloned()
                .unwrap_or_default();
            if original_sort_text.is_empty() {
                original_sort_text = SORT_TEXT_LOCATION_PRIORITY.to_string();
            }

            let sort_text = if is_deprecated(symbol, type_checker) {
                deprecate_sort_text(&original_sort_text)
            } else {
                original_sort_text
            };
            let entry = self.create_completion_item(
                ctx,
                type_checker,
                symbol,
                sort_text,
                replacement_token,
                data,
                position,
                file,
                name.clone(),
                needs_convert_property_access,
                origin,
                use_semicolons,
                compiler_options,
                is_member_completion,
            )?;
            let Some(entry) = entry else {
                continue;
            };

            // True for locals; false for globals, module exports from other files, `this.` completions.
            let is_local_or_type_only_alias = origin.is_none() || origin_is_type_only_alias(origin);
            let is_unowned_symbol_without_local_declaration = symbol.parent().is_none()
                && symbol
                    .declarations()
                    .iter()
                    .all(|&declaration| ast::get_source_file_of_node(declaration) != Some(file));
            let should_shadow_later_symbols =
                is_local_or_type_only_alias && !is_unowned_symbol_without_local_declaration;
            uniques.insert(name, should_shadow_later_symbols);
            let sym = if include_symbols { Some(symbol) } else { None };
            sorted_entries.push(CompletionItem { completion_item: entry, symbol: sym });
        }

        for auto_import in &data.auto_imports {
            // !!! check for type-only in JS
            // !!! deprecation

            let mut replacement_span: Option<lsproto::Range> = None;
            let mut insert_text = String::new();
            let mut filter_text = String::new();
            let mut is_snippet = false;
            let mut sort_text = SORT_TEXT_AUTO_IMPORT_SUGGESTIONS.to_string();

            if let Some(import_statement_completion) = &data.import_statement_completion {
                is_snippet = client_supports_item_snippet(ctx);
                (insert_text, replacement_span) =
                    get_insert_text_and_replacement_span_for_import_completion(
                        &auto_import.fix,
                        autoimport::get_import_kind_for_import_statement(
                            file,
                            &auto_import.export,
                            self.get_program(),
                        ),
                        import_statement_completion,
                        use_semicolons,
                        file,
                        &preferences,
                        is_snippet,
                    );
                // The edit range covers the whole import statement typed so far, and clients match that text against the
                // filter text, so it has to be the statement being inserted (as in Strada), not just the bare name.
                filter_text.clone_from(&insert_text);
                sort_text = SORT_TEXT_LOCATION_PRIORITY.to_string();
            }

            // Non-contextual keywords (e.g., `function`, `class`, `const`) cannot be used as identifiers,
            // so auto-imports with these names should not shadow keyword completions.
            let token = scanner::string_to_token(&auto_import.fix.name);
            if token != Kind::Unknown && ast::is_non_contextual_keyword(token) {
                continue;
            }

            if !auto_import.export.is_unresolved_alias() {
                if data.is_type_only_location {
                    if !auto_import.export.flags.intersects(SymbolFlags::Type)
                        && !auto_import.export.flags.intersects(SymbolFlags::Module)
                    {
                        continue;
                    }
                } else if data.import_statement_completion.is_none()
                    && !auto_import.export.flags.intersects(SymbolFlags::Value)
                {
                    continue;
                }
            }

            let mut entry = Self::create_lsp_completion_item(
                ctx,
                &auto_import.fix.name,
                &insert_text,
                &filter_text,
                &sort_text,
                auto_import.export.script_element_kind,
                auto_import.export.script_element_kind_modifiers,
                replacement_span,
                None,
                Some(lsproto::CompletionItemLabelDetails {
                    description: Some(auto_import.fix.module_specifier.clone()),
                    ..Default::default()
                }),
                file,
                position,
                false, /*isMemberCompletion*/
                is_snippet,
                data.import_statement_completion.is_none(), /*hasAction*/
                false,                                      /*preselect*/
                &auto_import.fix.module_specifier,
                Some(auto_import.fix.auto_import_fix.clone()),
                None, /*additionalTextEdits*/
                None, /*detail*/
            );

            entry.data.as_mut().unwrap().is_import_statement_completion =
                data.import_statement_completion.is_some();

            let is_shadowed = uniques.get(&auto_import.fix.name).copied().unwrap_or(false);
            if !is_shadowed {
                uniques.insert(auto_import.fix.name.clone(), false);
                sorted_entries.push(CompletionItem::new(entry));
            }
        }

        let mut unique_set: FxHashSet<String> =
            FxHashSet::with_capacity_and_hasher(uniques.len(), rustc_hash::FxBuildHasher);
        unique_set.extend(uniques.keys().map(std::clone::Clone::clone));
        Ok((unique_set, sorted_entries))
    }
}

// completions.go:2126
pub(crate) fn completion_name_for_literal(
    file: P<SourceFile>,
    preferences: &UserPreferences,
    literal: &LiteralValue,
) -> String {
    match literal {
        LiteralValue::String(literal) => quote(file, preferences, literal),
        LiteralValue::Number(literal) => {
            // Go `core.StringifyJson(literal, "", "")` (the error result for NaN/Inf is ignored, leaving "").
            json::marshal(&json::Value::Number(literal.0)).unwrap_or_default()
        }
        LiteralValue::BigInt(literal) => literal.string() + "n",
        LiteralValue::Boolean(literal) => panic!("Unhandled literal value: {literal}"),
    }
}

// completions.go:2143
fn get_insert_text_and_replacement_span_for_import_completion(
    fix: &autoimport::Fix,
    import_kind: lsproto::ImportKind,
    import_statement_completion: &ImportStatementCompletionInfo,
    use_semicolons: bool,
    file: P<SourceFile>,
    preferences: &UserPreferences,
    is_snippet: bool,
) -> (String, Option<lsproto::Range>) {
    let mut quoted_module_specifier = quote(file, preferences, &fix.module_specifier);
    if is_snippet {
        quoted_module_specifier =
            escape_snippet_text(&quoted_module_specifier.replace('\\', "\\\\"));
    }
    let tab_stop = if is_snippet { "$1" } else { "" };
    let suffix = if use_semicolons { ";" } else { "" };
    let top_level_type_only_text = if import_statement_completion.is_top_level_type_only {
        format!(" {} ", scanner::token_to_string(Kind::TypeKeyword))
    } else {
        " ".to_string()
    };
    let name = escape_snippet_text(&fix.name);
    let replacement_span = import_statement_completion.replacement_span;

    match import_kind {
        lsproto::ImportKind::COMMON_JS => (
            format!(
                "import{top_level_type_only_text}{name}{tab_stop} = require({quoted_module_specifier}){suffix}"
            ),
            replacement_span,
        ),
        lsproto::ImportKind::DEFAULT => (
            format!(
                "import{top_level_type_only_text}{name}{tab_stop} from {quoted_module_specifier}{suffix}"
            ),
            replacement_span,
        ),
        lsproto::ImportKind::NAMESPACE => (
            format!(
                "import{top_level_type_only_text}* as {name} from {quoted_module_specifier}{suffix}"
            ),
            replacement_span,
        ),
        lsproto::ImportKind::NAMED => (
            format!(
                "import{}{{ {}{}{} }} from {}{}",
                top_level_type_only_text,
                if import_statement_completion.could_be_type_only_import_specifier {
                    format!("{} ", scanner::token_to_string(Kind::TypeKeyword))
                } else {
                    String::new()
                },
                name,
                tab_stop,
                quoted_module_specifier,
                suffix
            ),
            replacement_span,
        ),
        _ => panic!("unhandled import kind: {}", import_kind.string()),
    }
}

// completions.go:2165
fn create_completion_item_for_literal(
    file: P<SourceFile>,
    preferences: &UserPreferences,
    literal: &LiteralValue,
) -> lsproto::CompletionItem {
    lsproto::CompletionItem {
        label: completion_name_for_literal(file, preferences, literal),
        kind: Some(lsproto::CompletionItemKind::CONSTANT),
        sort_text: Some(SORT_TEXT_LOCATION_PRIORITY.to_string()),
        commit_characters: Some(Vec::new()),
        ..Default::default()
    }
}

impl LanguageService {
    // completions.go:2178
    fn create_completion_item(
        &self,
        ctx: &Context,
        type_checker: &mut Checker,
        symbol: P<Symbol>,
        mut sort_text: SortText,
        replacement_token: Option<P<Node>>,
        data: &CompletionDataData,
        position: u32,
        file: P<SourceFile>,
        mut name: String,
        needs_convert_property_access: bool,
        origin: Option<&SymbolOriginInfo>,
        _use_semicolons: bool,
        _compiler_options: P<CompilerOptions>,
        is_member_completion: bool,
    ) -> Result<Option<lsproto::CompletionItem>, lsproto::Error> {
        let context_token = data.context_token;
        let mut insert_text = String::new();
        let mut filter_text = String::new();
        let mut replacement_span =
            self.get_replacement_range_for_context_token(file, replacement_token, position);
        let mut is_snippet = false;
        let mut has_action = false;
        let mut source = get_source_from_origin(origin).to_string();
        let mut label_details: Option<lsproto::CompletionItemLabelDetails> = None;
        let preferences = self.user_preferences().clone();
        let insert_question_dot = origin_is_nullable_member(origin);
        let use_braces = origin_is_symbol_member(origin) || needs_convert_property_access;
        if origin_is_this_type_node(origin) {
            if needs_convert_property_access {
                insert_text = format!(
                    "this{}[{}]",
                    if insert_question_dot { "?." } else { "" },
                    quote_property_name(file, &preferences, &name)
                );
            } else {
                insert_text =
                    format!("this{}{}", if insert_question_dot { "?." } else { "." }, name);
            }
        } else if data.property_access_to_convert.is_some() && (use_braces || insert_question_dot) {
            let property_access_to_convert = data.property_access_to_convert.unwrap();
            // We should only have needsConvertPropertyAccess if there's a property access to convert. But see microsoft/TypeScript#21790.
            // Somehow there was a global with a non-identifier name. Hopefully someone will complain about getting a "foo bar" global completion and provide a repro.
            if use_braces {
                if needs_convert_property_access {
                    insert_text = format!("[{}]", quote_property_name(file, &preferences, &name));
                } else {
                    insert_text = format!("[{name}]");
                }
            } else {
                insert_text.clone_from(&name);
            }

            if insert_question_dot || property_access_to_convert.question_dot_token().is_some() {
                insert_text = format!("?.{insert_text}");
            }

            let mut dot =
                astnav::find_child_of_kind(property_access_to_convert, Kind::DotToken, file);
            if dot.is_none() {
                dot = astnav::find_child_of_kind(
                    property_access_to_convert,
                    Kind::QuestionDotToken,
                    file,
                );
            }

            let Some(dot) = dot else {
                return Ok(None);
            };

            // If the text after the '.' starts with this name, write over it. Else, add new text.
            let end = if name.starts_with(property_access_to_convert.name().unwrap().text()) {
                property_access_to_convert.end()
            } else {
                dot.end()
            }
            .expect_source("parsed nodes have source positions");
            let (lsp_range, fidelity) = self.create_lsp_range_from_bounds(
                astnav::get_start_of_node(dot, file, false /*includeJSDoc*/),
                end,
                file,
            );
            if !fidelity.is_exact() {
                return Ok(None);
            }
            replacement_span = Some(lsp_range);
        }

        if data.jsx_initializer.is_initializer {
            if insert_text.is_empty() {
                insert_text.clone_from(&name);
            }
            insert_text = format!("{{{insert_text}}}");
            if let Some(initializer) = data.jsx_initializer.initializer {
                let (lsp_range, fidelity) = self.create_lsp_range_from_node(initializer, file);
                if !fidelity.is_exact() {
                    return Ok(None);
                }
                replacement_span = Some(lsp_range);
            }
        }

        if origin_is_promise(origin)
            && let Some(property_access_to_convert) = data.property_access_to_convert
        {
            if insert_text.is_empty() {
                insert_text.clone_from(&name);
            }
            let preceding_token = astnav::find_preceding_token(
                file,
                property_access_to_convert
                    .pos()
                    .expect_source("parsed nodes have source positions"),
            );
            let mut await_text = String::new();
            if let Some(preceding_token) = preceding_token
                && lsutil::position_is_asi_candidate(
                    preceding_token.end().expect_source("parsed tokens have source positions"),
                    preceding_token.parent().unwrap(),
                    file,
                )
            {
                await_text = ";".to_string();
            }

            let _ = write!(
                await_text,
                "(await {})",
                scanner::get_text_of_node(property_access_to_convert.expression().unwrap())
            );
            if needs_convert_property_access {
                insert_text = await_text + &insert_text;
            } else {
                let dot_str = if insert_question_dot { "?." } else { "." };
                insert_text = await_text + dot_str + &insert_text;
            }
            let is_in_await_expression =
                ast::is_await_expression(property_access_to_convert.parent().unwrap());
            let wrap_node = if is_in_await_expression {
                property_access_to_convert.parent().unwrap()
            } else {
                property_access_to_convert.expression().unwrap()
            };
            let (lsp_range, fidelity) = self.create_lsp_range_from_bounds(
                astnav::get_start_of_node(wrap_node, file, false /*includeJSDoc*/),
                property_access_to_convert
                    .end()
                    .expect_source("parsed nodes have source positions"),
                file,
            );
            if !fidelity.is_exact() {
                return Ok(None);
            }
            replacement_span = Some(lsp_range);
        }

        if origin_is_type_only_alias(origin) {
            has_action = true;
        }

        // Provide object member completions when missing commas, and insert missing commas.
        // For example:
        //
        //    interface I {
        //        a: string;
        //        b: number
        //     }
        //
        //     const cc: I = { a: "red" | }
        //
        // Completion should add a comma after "red" and provide completions for b
        if data.completion_kind == CompletionKind::ObjectPropertyDeclaration
            && let Some(context_token) = context_token
            && !ast::node_has_kind(
                astnav::find_preceding_token_ex(
                    file,
                    context_token.pos().expect_source("parsed tokens have source positions"),
                    Some(context_token),
                    false, /*excludeJSDoc*/
                ),
                Kind::CommaToken,
            )
        {
            let ct_parent = context_token.parent().unwrap();
            if ast::is_method_declaration(ct_parent.parent().unwrap())
                || ast::is_get_accessor_declaration(ct_parent.parent().unwrap())
                || ast::is_set_accessor_declaration(ct_parent.parent().unwrap())
                || ast::is_spread_assignment(ct_parent)
                || lsutil::get_last_token(
                    ast::find_ancestor(ct_parent, ast::is_property_assignment),
                    file,
                ) == Some(context_token)
                || ast::is_shorthand_property_assignment(ct_parent)
                    && get_line_of_position(
                        file,
                        context_token.end().expect_source("parsed tokens have source positions"),
                    ) != get_line_of_position(file, position)
            {
                source = COMPLETION_SOURCE_OBJECT_LITERAL_MEMBER_WITH_COMMA.to_string();
                has_action = true;
            }
        }

        let mut additional_text_edits: Option<Vec<lsproto::TextEdit>> = None;
        if preferences.include_completions_with_class_member_snippets.is_true()
            && data.completion_kind == CompletionKind::MemberLike
            && is_class_like_member_completion(symbol, data.location, file)
        {
            let member_completion_entry = self.get_entry_for_member_completion(
                ctx,
                type_checker,
                symbol,
                &name,
                data.location,
                position,
                context_token,
                file,
            )?;
            let Some(member_completion_entry) = member_completion_entry else {
                return Ok(None);
            };
            insert_text = member_completion_entry.insert_text;
            filter_text = member_completion_entry.filter_text;
            is_snippet = member_completion_entry.is_snippet;
            if !member_completion_entry.additional_text_edits.is_empty() {
                additional_text_edits = Some(member_completion_entry.additional_text_edits);
                has_action = true;
                source = COMPLETION_SOURCE_CLASS_MEMBER_SNIPPET.to_string();
            }
        }

        if origin_is_object_literal_method(origin) {
            let method = origin.unwrap().as_object_literal_method();
            insert_text.clone_from(&method.insert_text);
            is_snippet = method.is_snippet;
            label_details.clone_from(&method.label_details);
            if !client_supports_item_label_details(ctx) {
                name.push_str(method.label_details.as_ref().unwrap().detail.as_ref().unwrap());
                label_details = None;
            }
            source = COMPLETION_SOURCE_OBJECT_LITERAL_METHOD_SNIPPET.to_string();
            sort_text = sort_below(&sort_text);
        }

        if data.is_jsx_identifier_expected
            && !data.is_right_of_open_tag
            && client_supports_item_snippet(ctx)
            && preferences.jsx_attribute_completion_style != JsxAttributeCompletionStyle::None
            && !(data.location.parent().is_some()
                && ast::is_jsx_attribute(data.location.parent().unwrap())
                && data.location.parent().unwrap().initializer().is_some())
        {
            let mut use_braces =
                preferences.jsx_attribute_completion_style == JsxAttributeCompletionStyle::Braces;
            let t =
                type_checker.get_type_of_symbol_at_location(symbol, Some(data.location)).unwrap();

            // If is boolean like or undefined, don't return a snippet, we want to return just the completion.
            if preferences.jsx_attribute_completion_style == JsxAttributeCompletionStyle::Auto
                && !t.is_boolean_like()
                && !(t.is_union() && t.types().iter().any(|t| t.is_boolean_like()))
            {
                if t.is_string_like()
                    || t.is_union()
                        && t.types().iter().all(|&t| {
                            t.flags().intersects(TypeFlags::StringLike | TypeFlags::Undefined)
                                || is_string_and_empty_anonymous_object_intersection(
                                    type_checker,
                                    t,
                                )
                        })
                {
                    // If type is string-like or undefined, use quotes.
                    insert_text = format!(
                        "{}={}",
                        escape_snippet_text(&name),
                        quote(file, &preferences, "$1")
                    );
                    is_snippet = true;
                } else {
                    // Use braces for everything else.
                    use_braces = true;
                }
            }

            if use_braces {
                insert_text = escape_snippet_text(&name) + "={$1}";
                is_snippet = true;
            }
        }

        let parent_named_import_or_export =
            ast::find_ancestor(data.location, is_named_imports_or_exports);
        if let Some(parent_named_import_or_export) = parent_named_import_or_export {
            if !scanner::is_identifier_text(&name, LanguageVariant::Standard) {
                insert_text = quote_property_name(file, &preferences, &name);

                if parent_named_import_or_export.kind() == Kind::NamedImports {
                    // Check if it is `import { ^here as name } from '...'``.
                    // We have to access the scanner here to check if it is `{ ^here as name }`` or `{ ^here, as, name }`.
                    let mut scanner = scanner::Scanner::new();
                    scanner.set_text(file.text());
                    scanner.reset_pos(position);
                    if !(scanner.scan() == Kind::AsKeyword && scanner.scan() == Kind::Identifier) {
                        let _ = write!(
                            insert_text,
                            " as {}",
                            generate_identifier_for_arbitrary_string(&name)
                        );
                    }
                }
            } else if parent_named_import_or_export.kind() == Kind::NamedImports {
                let possible_token = scanner::string_to_token(&name);
                if possible_token != Kind::Unknown
                    && (possible_token == Kind::AwaitKeyword
                        || lsutil::is_non_contextual_keyword(possible_token))
                {
                    insert_text = format!("{name} as {name}_");
                }
            }
        }

        // Commit characters

        let element_kind = lsutil::get_symbol_kind(Some(type_checker), symbol, data.location);
        let mut commit_characters: Option<Vec<String>> = None;
        if client_supports_item_commit_characters(ctx) {
            if element_kind == lsutil::ScriptElementKind::Warning
                || element_kind == lsutil::ScriptElementKind::String
            {
                commit_characters = Some(Vec::new());
            } else if !client_supports_default_commit_characters(ctx) {
                commit_characters.clone_from(&data.default_commit_characters);
            }
            // Otherwise use the completion list default.
        }

        let preselect =
            is_recommended_completion_match(symbol, data.recommended_completion, type_checker);
        let kind_modifiers = lsutil::get_symbol_modifiers(Some(type_checker), Some(symbol));

        Ok(Some(Self::create_lsp_completion_item(
            ctx,
            &name,
            &insert_text,
            &filter_text,
            &sort_text,
            element_kind,
            kind_modifiers,
            replacement_span,
            commit_characters,
            label_details,
            file,
            position,
            is_member_completion,
            is_snippet,
            has_action,
            preselect,
            &source,
            None, /*autoImportFix*/
            additional_text_edits,
            None, /*detail*/
        )))
    }
}

// completions.go:2467
struct MemberCompletionEntry {
    insert_text: String,
    filter_text: String,
    is_snippet: bool,
    additional_text_edits: Vec<lsproto::TextEdit>,
}

impl LanguageService {
    // completions.go:2474
    fn get_entry_for_object_literal_method_completion(
        &self,
        ctx: &Context,
        type_checker: &mut Checker,
        symbol: P<Symbol>,
        enclosing_declaration: P<Node>,
        file: P<SourceFile>,
    ) -> Option<SymbolOriginInfoObjectLiteralMethod> {
        let mut snippet_printer = create_snippet_printer(
            printer::PrinterOptions {
                remove_comments: true,
                new_line: tsrs_core::get_new_line_kind(&self.format_options().new_line_character),
                target: self.get_program().options().get_emit_script_target(),
                ..Default::default()
            },
            None, /*emitContext*/
        );

        let is_snippet = client_supports_item_snippet(ctx);
        let method = self.create_object_literal_method(
            &snippet_printer,
            type_checker,
            symbol,
            enclosing_declaration,
            file,
            is_snippet,
        )?;

        let mut insert_text = snippet_printer.print_and_format_node_with_settings(
            ctx,
            method,
            file,
            change::get_format_code_settings_for_writing(self.format_options(), file),
        );
        insert_text += ",";

        Some(SymbolOriginInfoObjectLiteralMethod {
            insert_text,
            label_details: Some(lsproto::CompletionItemLabelDetails {
                detail: Some(self.print_object_literal_method_label_detail(
                    method,
                    file,
                    &snippet_printer.factory,
                )),
                ..Default::default()
            }),
            is_snippet,
        })
    }

    // completions.go:2499
    fn create_object_literal_method(
        &self,
        snippet_printer: &SnippetPrinter,
        type_checker: &mut Checker,
        symbol: P<Symbol>,
        enclosing_declaration: P<Node>,
        file: P<SourceFile>,
        is_snippet: bool,
    ) -> Option<P<Node>> {
        let factory = &snippet_printer.factory;
        let emit_context = snippet_printer.emit_context;

        let declaration = symbol.declarations().first().copied();
        if !is_object_literal_method_completion_candidate_declaration(declaration) {
            return None;
        }
        let declaration = declaration.unwrap();

        let type_of_symbol = type_checker
            .get_type_of_symbol_at_location(symbol, Some(enclosing_declaration))
            .unwrap();
        let mut effective_type = type_checker.get_widened_type_exported(type_of_symbol);
        if effective_type.flags().intersects(TypeFlags::Union) && effective_type.types().len() < 10
        {
            effective_type = type_checker
                .get_union_type_ex_exported(effective_type.types(), UnionReduction::Subtype);
        }
        if effective_type.flags().intersects(TypeFlags::Union) {
            let mut function_type: Option<P<Type>> = None;
            for &union_type in effective_type.types() {
                if type_checker
                    .get_signatures_of_type_exported(union_type, SignatureKind::Call)
                    .is_empty()
                {
                    continue;
                }
                if function_type.is_some() {
                    return None;
                }
                function_type = Some(union_type);
            }
            effective_type = function_type?;
        }

        let signatures =
            type_checker.get_signatures_of_type_exported(effective_type, SignatureKind::Call);
        if signatures.len() != 1 {
            return None;
        }

        let mut flags = Flags::OmitThisParameter;
        if lsutil::get_quote_preference(file, self.user_preferences()) == QuotePreference::Single {
            flags |= Flags::UseSingleQuotesForStringLiteralType;
        }
        let type_node = type_checker.type_to_type_node(
            effective_type,
            Some(enclosing_declaration),
            flags,
            None, /*idToSymbol*/
        )?;
        if type_node.kind() != Kind::FunctionType {
            return None;
        }

        let type_parameters_nodes = type_node.parameters();
        let mut parameters: Vec<P<Node>> = Vec::with_capacity(type_parameters_nodes.len());
        parameters.extend(type_parameters_nodes.iter().map(|&parameter| {
            factory.new_parameter_declaration(
                None, /*modifiers*/
                parameter.as_parameter_declaration().dot_dot_dot_token(),
                parameter.name().unwrap().clone_node(factory),
                None, /*questionToken*/
                None, /*typeNode*/
                parameter.as_parameter_declaration().initializer(),
            )
        }));

        let mut body =
            factory.new_block(factory.new_node_list(Vec::new() /*nodes*/), true /*multiLine*/);
        if is_snippet {
            body = create_snippet_tab_stop_body(factory, emit_context);
        }

        Some(factory.new_method_declaration(
            None, /*modifiers*/
            None, /*asteriskToken*/
            declaration.name().unwrap().clone_node(factory),
            None, /*postfixToken*/
            None, /*typeParameters*/
            Some(factory.new_node_list(parameters)),
            None, /*typeNode*/
            None, /*fullSignature*/
            Some(body),
        ))
    }
}

// completions.go:2573
fn is_object_literal_method_completion_candidate_declaration(declaration: Option<P<Node>>) -> bool {
    let Some(declaration) = declaration else {
        return false;
    };
    matches!(
        declaration.kind(),
        Kind::PropertySignature
            | Kind::PropertyDeclaration
            | Kind::MethodSignature
            | Kind::MethodDeclaration
    )
}

// completions.go:2585
pub(crate) struct ObjectLiteralMethodSymbol {
    pub(crate) symbol: P<Symbol>,
    pub(crate) origin: SymbolOriginInfo,
}

impl LanguageService {
    // completions.go:2590
    pub(crate) fn collect_object_literal_method_symbols(
        &self,
        ctx: &Context,
        type_checker: &mut Checker,
        members: &[P<Symbol>],
        enclosing_declaration: P<Node>,
        file: P<SourceFile>,
    ) -> Vec<ObjectLiteralMethodSymbol> {
        if ast::is_source_file_js(file) {
            return Vec::new();
        }

        let preferences = self.user_preferences().clone();
        let mut methods = Vec::new();
        methods.extend(members.iter().filter_map(|&member| {
            if !is_object_literal_method_symbol(member) {
                return None;
            }
            let (display_name, _) = get_completion_entry_display_name_for_symbol(
                file,
                &preferences,
                member,
                None, /*origin*/
                CompletionKind::ObjectPropertyDeclaration,
                false, /*isJsxIdentifierExpected*/
            );
            if display_name.is_empty() {
                return None;
            }
            let entry = self.get_entry_for_object_literal_method_completion(
                ctx,
                type_checker,
                member,
                enclosing_declaration,
                file,
            )?;
            Some(ObjectLiteralMethodSymbol {
                symbol: member,
                origin: SymbolOriginInfo {
                    kind: symbolOriginInfoKind::ObjectLiteralMethod,
                    data: SymbolOriginData::ObjectLiteralMethod(entry),
                },
            })
        }));
        methods
    }
}

// completions.go:2620
fn is_object_literal_method_symbol(symbol: P<Symbol>) -> bool {
    symbol.flags().intersects(SymbolFlags::Property | SymbolFlags::Method)
}

impl LanguageService {
    // completions.go:2624
    fn print_object_literal_method_label_detail(
        &self,
        method: P<Node>,
        file: P<SourceFile>,
        factory: &NodeFactory,
    ) -> String {
        let method_signature = factory.new_method_signature_declaration(
            None, /*modifiers*/
            factory.new_identifier(""),
            method.postfix_token(),
            method.type_parameter_list(),
            method.parameter_list(),
            method.type_node(),
        );
        let mut signature_printer = printer::new_printer(
            printer::PrinterOptions {
                remove_comments: true,
                omit_trailing_semicolon: true,
                new_line: tsrs_core::get_new_line_kind(&self.format_options().new_line_character),
                target: self.get_program().options().get_emit_script_target(),
                ..Default::default()
            },
            printer::PrintHandlers::default(),
            None, /*emitContext*/
        );
        signature_printer.emit(method_signature, Some(file))
    }

    // completions.go:2643
    fn get_entry_for_member_completion(
        &self,
        ctx: &Context,
        type_checker: &mut Checker,
        symbol: P<Symbol>,
        name: &str,
        location: P<Node>,
        position: u32,
        context_token: Option<P<Node>>,
        file: P<SourceFile>,
    ) -> Result<Option<MemberCompletionEntry>, lsproto::Error> {
        let Some(class_like_declaration) = ast::find_ancestor(location, ast::is_class_like) else {
            return Ok(None);
        };

        let mut import_adder = self.create_import_adder(ctx, type_checker, file)?;

        let change_tracker = change::new_tracker(
            ctx,
            &self.get_program().options(),
            self.format_options(),
            Arc::clone(&self.converters),
        );

        let present_modifiers = self.get_present_member_modifiers(context_token, file, position);
        let abstract_ = present_modifiers.modifiers.intersects(ModifierFlags::Abstract)
            && class_like_declaration.modifier_flags().intersects(ModifierFlags::Abstract);
        let is_snippet = client_supports_item_snippet(ctx);
        let mut body = change_tracker.node_factory.new_block(
            change_tracker.node_factory.new_node_list(Vec::new()),
            true, /*multiLine*/
        );
        if is_snippet {
            body = create_snippet_tab_stop_body(
                &change_tracker.node_factory,
                change_tracker.emit_context,
            );
        }

        let nodes = {
            let mut fixer = new_missing_member_fixer(
                &change_tracker,
                self.get_program(),
                type_checker,
                self.user_preferences().clone(),
                import_adder.as_deref_mut(),
                locale_from_context(ctx),
            );
            fixer.create_member_from_symbol(
                symbol,
                class_like_declaration,
                file,
                Some(body),
                preserveOptionalFlags::Property,
                abstract_,
            )
        };
        let mut additional_text_edits: Vec<lsproto::TextEdit> = Vec::new();
        if let Some(import_adder) = import_adder.as_mut()
            && import_adder.has_fixes()
        {
            additional_text_edits = import_adder.edits();
        }
        if let Some(erase_range) = present_modifiers.erase_range {
            additional_text_edits
                .push(lsproto::TextEdit { range: erase_range, new_text: String::new() });
        }

        let mut modifiers = ModifierFlags::None;
        let mut completion_nodes: Vec<P<Node>> = Vec::with_capacity(nodes.len());
        for node in nodes {
            if completion_nodes.is_empty() {
                modifiers = node.modifier_flags();
                if abstract_ {
                    modifiers |= ModifierFlags::Abstract;
                }
                if ast::is_class_element(node)
                    && type_checker.get_member_override_modifier_status_exported(
                        class_like_declaration,
                        node,
                        Some(symbol),
                    ) == MemberOverrideStatus::NeedsOverride
                {
                    modifiers |= ModifierFlags::Override;
                }
            }
            completion_nodes.push(node);
        }

        if completion_nodes.is_empty() {
            return Ok(Some(MemberCompletionEntry {
                insert_text: name.to_string(),
                filter_text: name.to_string(),
                is_snippet,
                additional_text_edits,
            }));
        }

        let mut allowed_modifiers = modifiers | ModifierFlags::Override | ModifierFlags::Public;
        if symbol.flags().intersects(SymbolFlags::Method) {
            allowed_modifiers |= ModifierFlags::Async;
        } else {
            allowed_modifiers |= ModifierFlags::Ambient | ModifierFlags::Readonly;
        }

        let allowed_and_present = present_modifiers.modifiers & allowed_modifiers;
        if !(present_modifiers.modifiers & !allowed_modifiers).is_empty() {
            return Ok(None);
        }

        if modifiers.intersects(ModifierFlags::Protected)
            && allowed_and_present.intersects(ModifierFlags::Public)
        {
            modifiers &= !ModifierFlags::Protected;
        }

        if allowed_and_present != ModifierFlags::None
            && !allowed_and_present.intersects(ModifierFlags::Public)
        {
            modifiers &= !ModifierFlags::Public;
        }

        modifiers |= allowed_and_present;
        let new_line = self.format_options().new_line_character.clone();
        let mut snippet_printer = create_snippet_printer(
            printer::PrinterOptions {
                remove_comments: true,
                new_line: tsrs_core::get_new_line_kind(&new_line),
                target: self.get_program().options().get_emit_script_target(),
                ..Default::default()
            },
            Some(change_tracker.emit_context),
        );

        let mut decorated_node: Option<P<Node>> = None;
        if !present_modifiers.decorators.is_empty() {
            let last_node_index = completion_nodes.len() - 1;
            if ast::can_have_decorators(completion_nodes[last_node_index]) {
                decorated_node = Some(completion_nodes[last_node_index]);
            }
        }

        let mut texts: Vec<String> = Vec::with_capacity(completion_nodes.len());
        texts.extend(completion_nodes.into_iter().map(|node| {
            let decorators: &[P<Node>] =
                if Some(node) == decorated_node { &present_modifiers.decorators } else { &[] };
            let node = ast::replace_modifiers(
                &change_tracker.node_factory,
                node,
                create_modifier_list(&change_tracker.node_factory, modifiers, decorators),
            );

            snippet_printer.print_and_format_node_with_settings(
                ctx,
                node,
                file,
                change::get_format_code_settings_for_writing(self.format_options(), file),
            )
        }));

        let insert_text = texts.join(&new_line);
        if insert_text.is_empty() {
            return Ok(None);
        }

        Ok(Some(MemberCompletionEntry {
            insert_text,
            filter_text: name.to_string(),
            is_snippet,
            additional_text_edits,
        }))
    }
}

// completions.go:2760
#[derive(Default)]
pub struct PresentMemberModifiers {
    modifiers: ModifierFlags,
    decorators: Vec<P<Node>>,
    erase_range: Option<lsproto::Range>,
}

impl LanguageService {
    // completions.go:2766
    fn get_present_member_modifiers(
        &self,
        context_token: Option<P<Node>>,
        file: P<SourceFile>,
        position: u32,
    ) -> PresentMemberModifiers {
        let Some(context_token) = context_token else {
            return PresentMemberModifiers::default();
        };
        if get_line_of_position(file, position)
            > get_line_of_position(
                file,
                context_token.end().expect_source("parsed tokens have source positions"),
            )
        {
            return PresentMemberModifiers::default();
        }

        let mut modifiers = ModifierFlags::None;
        let mut decorators: Vec<P<Node>> = Vec::new();
        let mut range_pos = position;
        let mut range_end = position;

        let ct_parent = context_token.parent().unwrap();
        if ast::is_property_declaration(ct_parent) {
            let context_modifier_kind = modifier_like_kind(Some(context_token));
            if context_modifier_kind == Kind::Unknown {
                return PresentMemberModifiers::default();
            }

            let modifier_nodes = ct_parent.modifier_nodes();
            if !modifier_nodes.is_empty() {
                modifiers |= ast::modifiers_to_flags(modifier_nodes) & ModifierFlags::Modifier;
                for &modifier in modifier_nodes {
                    if ast::is_decorator(modifier) {
                        decorators.push(modifier);
                    }
                    range_pos = range_pos.min(scanner::get_token_pos_of_node(
                        modifier, file, false, /*includeJSDoc*/
                    ));
                }
            }

            let context_modifier_flag = ast::modifier_to_flag(context_modifier_kind);
            if !modifiers.intersects(context_modifier_flag) {
                modifiers |= context_modifier_flag;
                range_pos = range_pos.min(astnav::get_start_of_node(
                    context_token,
                    file,
                    false, /*includeJSDoc*/
                ));
            }

            if ct_parent.name() != Some(context_token) {
                range_end = astnav::get_start_of_node(
                    ct_parent.name().unwrap(),
                    file,
                    false, /*includeJSDoc*/
                );
            }
        }

        let mut erase_range: Option<lsproto::Range> = None;
        if range_pos < range_end {
            let (lsp_range, fidelity) =
                self.create_lsp_range_from_bounds(range_pos, range_end, file);
            if fidelity.is_exact() {
                erase_range = Some(lsp_range);
            }
        }

        PresentMemberModifiers { modifiers, decorators, erase_range }
    }
}

// completions.go:2819
fn modifier_like_kind(node: Option<P<Node>>) -> Kind {
    let Some(node) = node else {
        return Kind::Unknown;
    };
    if ast::is_modifier(node) {
        return node.kind();
    }
    if ast::is_identifier(node) {
        let keyword_kind = scanner::identifier_to_keyword_kind(node);
        if keyword_kind != Kind::Unknown && ast::is_modifier_kind(keyword_kind) {
            return keyword_kind;
        }
    }
    Kind::Unknown
}

// completions.go:2835
fn create_modifier_list(
    factory: &NodeFactory,
    flags: ModifierFlags,
    decorators: &[P<Node>],
) -> Option<P<ast::ModifierList>> {
    let mut nodes: Vec<P<Node>> = Vec::new();
    nodes.extend(decorators.iter().map(|&decorator| decorator.clone_node(factory)));
    nodes.extend(ast::create_modifiers_from_modifier_flags(flags, |k| factory.new_modifier(k)));
    if nodes.is_empty() {
        return None;
    }
    Some(factory.new_modifier_list(nodes))
}

// completions.go:2847
fn create_snippet_tab_stop_body(factory: &NodeFactory, emit_context: P<EmitContext>) -> P<Node> {
    let empty_statement = factory.new_empty_statement();
    emit_context.set_snippet_element(
        empty_statement,
        SnippetElement { kind: SnippetKind::TabStop, order: 0 },
    );
    factory.new_block(factory.new_node_list(vec![empty_statement]), true /*multiLine*/)
}

impl LanguageService {
    // completions.go:2856
    fn create_import_adder(
        &self,
        ctx: &Context,
        type_checker: &mut Checker,
        file: P<SourceFile>,
    ) -> Result<Option<Box<dyn ImportAdder>>, lsproto::Error> {
        if tspath::is_dynamic_file_name(file.file_name()) {
            return Ok(None);
        }
        let view = self.get_prepared_auto_import_view(file, type_checker)?;
        // Go checks the view for nil; a prepared view is never nil.
        Ok(Some(autoimport::new_import_adder(
            ctx,
            self.get_program(),
            type_checker,
            file,
            view,
            self.format_options(),
            Arc::clone(&self.converters),
            self.user_preferences().clone(),
        )))
    }
}

// completions.go:2870
fn is_recommended_completion_match(
    local_symbol: P<Symbol>,
    recommended_completion: Option<P<Symbol>>,
    type_checker: &mut Checker,
) -> bool {
    Some(local_symbol) == recommended_completion
        || local_symbol.flags().intersects(SymbolFlags::ExportValue)
            && Some(type_checker.get_export_symbol_of_symbol(local_symbol))
                == recommended_completion
}

// Ported from vscode.
// completions.go:2876
fn is_word_separator(r: char) -> bool {
    matches!(
        r,
        '`' | '~'
            | '!'
            | '@'
            | '%'
            | '^'
            | '&'
            | '*'
            | '('
            | ')'
            | '-'
            | '='
            | '+'
            | '['
            | '{'
            | ']'
            | '}'
            | '\\'
            | '|'
            | ';'
            | ':'
            | '\''
            | '"'
            | ','
            | '.'
            | '<'
            | '>'
            | '/'
            | '?'
    )
}

// Finds the length and first rune of the word that ends at the given position.
// e.g. for "abc def.ghi|jkl", the word length is 3 and the word start is 'g'.
// completions.go:2883
pub(crate) fn get_word_length_and_start(
    source_file: P<SourceFile>,
    position: u32,
) -> (usize, char) {
    // !!! Port other case of vscode's `DEFAULT_WORD_REGEXP` that covers words that start like numbers, e.g. -123.456abcd.
    let text = &source_file.text()[..position as usize];
    let mut total_size = 0;
    let mut first_rune = '\0';
    for r in text.chars().rev() {
        if is_word_separator(r) || r.is_whitespace() {
            break;
        }
        total_size += r.len_utf8();
        first_rune = r;
    }
    // If word starts with `@`, disregard this first character.
    if first_rune == '@' {
        total_size -= 1;
        first_rune = text[text.len() - total_size..].chars().next().unwrap_or('\u{FFFD}');
    }
    (total_size, first_rune)
}

// `["ab c"]` -> `ab c`
// `['ab c']` -> `ab c`
// `[123]` -> `123`
// completions.go:2906
fn trim_element_access(text: &str) -> String {
    let mut text = text.strip_prefix('[').unwrap_or(text);
    text = text.strip_suffix(']').unwrap_or(text);
    if text.starts_with('\'') && text.ends_with('\'') {
        let t = text.strip_suffix('\'').unwrap_or(text);
        text = t.strip_prefix('\'').unwrap_or(t);
    }
    if text.starts_with('"') && text.ends_with('"') {
        let t = text.strip_suffix('"').unwrap_or(text);
        text = t.strip_prefix('"').unwrap_or(t);
    }
    text.to_string()
}

// Ported from vscode ts extension: `getFilterText`.
// completions.go:2919
pub(crate) fn get_filter_text(
    _file: P<SourceFile>,
    _position: u32,
    insert_text: &str,
    label: &str,
    word_start: char,
    dot_accessor: &str,
) -> String {
    // Private field completion, e.g. label `#bar`.
    if let Some(after) = label.strip_prefix('#') {
        if !insert_text.is_empty() {
            if let Some(after) = insert_text.strip_prefix("this.#") {
                if word_start == '#' {
                    // `method() { this.#| }`
                    // `method() { #| }`
                    return String::new();
                }
                // `method() { this.| }`
                // `method() { | }`
                return after.to_string();
            }
        } else if word_start == '#' {
            // `method() { this.#| }`
            return String::new();
        } else {
            // `method() { this.| }`
            // `method() { | }`
            return after.to_string();
        }
    }

    // For `this.` completions, generally don't set the filter text since we don't want them to be overly deprioritized. microsoft/vscode#74164
    if insert_text.starts_with("this.") {
        return String::new();
    }

    // Handle the case:
    // ```
    // const xyz = { 'ab c': 1 };
    // xyz.ab|
    // ```
    // In which case we want to insert a bracket accessor but should use `.abc` as the filter text instead of
    // the bracketed insert text.
    if insert_text.starts_with('[') {
        return dot_accessor.to_string() + &trim_element_access(insert_text);
    }

    if let Some(insert_text) = insert_text.strip_prefix("?.") {
        // Handle this case like the case above:
        // ```
        // const xyz = { 'ab c': 1 } | undefined;
        // xyz.ab|
        // ```
        // filterText should be `.ab c` instead of `?.['ab c']`.
        if insert_text.starts_with('[') {
            return dot_accessor.to_string() + &trim_element_access(insert_text);
        }
        // ```
        // const xyz = { abc: 1 } | undefined;
        // xyz.ab|
        // ```
        // filterText should be `.abc` instead of `?.abc.
        return dot_accessor.to_string() + insert_text;
    }

    // In all other cases, fall back to using the insertText.
    insert_text.to_string()
}

// Ported from vscode's `provideCompletionItems`.
// completions.go:2993
pub(crate) fn get_dot_accessor(file: P<SourceFile>, position: u32) -> String {
    let text = &file.text()[..position as usize];
    let mut total_size = 0;
    if text.ends_with("?.") {
        total_size += 2;
        return file.text()[(position - total_size) as usize..position as usize].to_string();
    }
    if text.ends_with('.') {
        total_size += 1;
        return file.text()[(position - total_size) as usize..position as usize].to_string();
    }
    String::new()
}

// completions.go:3014
pub(crate) fn str_ptr_to(v: &str) -> Option<String> {
    if v.is_empty() {
        return None;
    }
    Some(v.to_string())
}

// completions.go:3021
pub(crate) fn bool_to_ptr(v: bool) -> Option<bool> {
    if v {
        return Some(true);
    }
    None
}

// completions.go:3028
pub(crate) fn get_line_of_position(file: P<SourceFile>, pos: u32) -> usize {
    scanner::get_ecma_line_of_position(&*file, pos)
}

// completions.go:3033
pub(crate) fn get_line_end_of_position(file: P<SourceFile>, pos: u32) -> u32 {
    let line = get_line_of_position(file, pos);
    let line_starts = scanner::get_ecma_line_starts(&*file);
    let last_char_pos = if line + 1 >= line_starts.len() {
        file.as_node().end().expect_source("parsed source files have source positions")
    } else {
        // Go subtracts one byte for the line break, which lands inside the three-byte UTF-8 encoding of U+2028 /
        // U+2029; end the line before the whole line break so the callers' slices stay on character boundaries.
        let next_line_start = line_starts[line + 1];
        let line_break = file.text()[..next_line_start as usize]
            .chars()
            .next_back()
            .expect("a line start after 0 follows a line break");
        next_line_start - u32::try_from(line_break.len_utf8()).expect("a char is at most 4 bytes")
    };
    let full_text = file.text().as_bytes();
    if last_char_pos > 0
        && (last_char_pos as usize) < full_text.len()
        && full_text[last_char_pos as usize] == b'\n'
        && full_text[(last_char_pos - 1) as usize] == b'\r'
    {
        return last_char_pos - 1;
    }
    last_char_pos
}

// completions.go:3049
fn is_class_like_member_completion(
    symbol: P<Symbol>,
    location: P<Node>,
    file: P<SourceFile>,
) -> bool {
    if ast::is_in_js_file(location) {
        return false;
    }
    let member_flags = SymbolFlags::ClassMember & SymbolFlags::EnumMemberExcludes;
    symbol.flags().intersects(member_flags)
        && (ast::is_class_like(location)
            || (location.parent().is_some()
                && location.parent().unwrap().parent().is_some()
                && ast::is_class_element(location.parent().unwrap())
                && Some(location) == location.parent().unwrap().name()
                && lsutil::get_last_token(location.parent(), file)
                    == location.parent().unwrap().name()
                && ast::is_class_like(location.parent().unwrap().parent().unwrap()))
            || (location.parent().is_some()
                && ast::is_syntax_list(location)
                && ast::is_class_like(location.parent().unwrap())))
}

// completions.go:3060
fn symbol_appears_to_be_type_only(symbol: P<Symbol>, type_checker: &mut Checker) -> bool {
    let flags = checker::skip_alias(symbol, type_checker).combined_local_and_export_symbol_flags();
    !flags.intersects(SymbolFlags::Value)
        && (symbol.declarations().is_empty()
            || !ast::is_in_js_file(symbol.declarations()[0])
            || flags.intersects(SymbolFlags::Type))
}

// completions.go:3066
fn should_include_symbol(
    symbol: P<Symbol>,
    data: &CompletionDataData,
    closest_symbol_declaration: Option<P<Node>>,
    file: P<SourceFile>,
    type_checker: &mut Checker,
    compiler_options: &CompilerOptions,
) -> bool {
    let mut all_flags = symbol.flags();
    let location = data.location;
    // export = /**/ here we want to get all meanings, so any symbol is ok
    if location.parent().is_some() && ast::is_export_assignment(location.parent().unwrap()) {
        return true;
    }

    // Filter out variables from their own initializers
    // `const a = /* no 'a' here */`
    if let Some(closest_symbol_declaration) = closest_symbol_declaration
        && ast::is_variable_declaration(closest_symbol_declaration)
        && symbol.value_declaration() == Some(closest_symbol_declaration)
    {
        return false;
    }

    // Filter out current and latter parameters from defaults
    // `function f(a = /* no 'a' and 'b' here */, b) { }` or
    // `function f<T = /* no 'T' and 'T2' here */>(a: T, b: T2) { }`
    let mut symbol_declaration: Option<P<Node>> = None;
    if symbol.value_declaration().is_some() {
        symbol_declaration = symbol.value_declaration();
    } else if !symbol.declarations().is_empty() {
        symbol_declaration = Some(symbol.declarations()[0]);
    }

    if let (Some(closest_symbol_declaration), Some(symbol_declaration)) =
        (closest_symbol_declaration, symbol_declaration)
    {
        if ast::is_parameter_declaration(closest_symbol_declaration)
            && ast::is_parameter_declaration(symbol_declaration)
        {
            let parameters = closest_symbol_declaration.parent().unwrap().parameter_list().unwrap();
            #[expect(
                clippy::suspicious_operation_groupings,
                reason = "both bounds intentionally compare declaration start positions"
            )]
            if symbol_declaration.pos() >= closest_symbol_declaration.pos()
                && symbol_declaration.pos() < parameters.end()
            {
                return false;
            }
        } else if ast::is_type_parameter_declaration(closest_symbol_declaration)
            && ast::is_type_parameter_declaration(symbol_declaration)
        {
            if closest_symbol_declaration == symbol_declaration
                && data.context_token.is_some()
                && data.context_token.unwrap().kind() == Kind::ExtendsKeyword
            {
                // filter out the directly self-recursive type parameters
                // `type A<K extends /* no 'K' here*/> = K`
                return false;
            }
            if is_in_type_parameter_default(data.context_token)
                && !ast::is_infer_type_node(closest_symbol_declaration.parent().unwrap())
            {
                let type_parameters =
                    closest_symbol_declaration.parent().unwrap().type_parameter_list();
                if let Some(type_parameters) = type_parameters {
                    #[expect(
                        clippy::suspicious_operation_groupings,
                        reason = "both bounds intentionally compare declaration start positions"
                    )]
                    if symbol_declaration.pos() >= closest_symbol_declaration.pos()
                        && symbol_declaration.pos() < type_parameters.end()
                    {
                        return false;
                    }
                }
            }
        }
    }

    // External modules can have global export declarations that will be
    // available as global keywords in all scopes. But if the external module
    // already has an explicit export and user only wants to use explicit
    // module imports then the global keywords will be filtered out so auto
    // import suggestions will win in the completion.
    let symbol_origin = checker::skip_alias(symbol, type_checker);
    // We only want to filter out the global keywords.
    // Auto Imports are not available for scripts so this conditional is always false.
    if file.external_module_indicator().is_some()
        && compiler_options.allow_umd_global_access != Tristate::True
        && symbol != symbol_origin
        && data
            .symbol_to_sort_text_map
            .get(&ast::get_symbol_id(symbol))
            .map(std::string::String::as_str)
            == Some(SORT_TEXT_GLOBALS_OR_KEYWORDS)
        && symbol.parent().is_some()
        && checker::is_external_module_symbol(symbol.parent().unwrap())
    {
        return false;
    }

    all_flags |= symbol_origin.combined_local_and_export_symbol_flags();
    if symbol.flags().intersects(SymbolFlags::Alias) {
        all_flags |= type_checker.get_symbol_flags_exported(symbol);
    }

    // import m = /**/ <-- It can only access namespace (if typing import = x. this would get member symbols and not namespace)
    if crate::utilities::is_in_right_side_of_internal_import_equals_declaration(data.location) {
        return all_flags.intersects(SymbolFlags::Namespace);
    }

    if data.is_type_only_location {
        // It's a type, but you can reach it by namespace.type as well.
        return symbol_can_be_referenced_at_type_location(symbol, type_checker, None);
    }

    // expressions are value space (which includes the value namespaces)
    all_flags.intersects(SymbolFlags::Value)
}

// completions.go:3158
pub(crate) fn get_completion_entry_display_name_for_symbol(
    file: P<SourceFile>,
    preferences: &UserPreferences,
    symbol: P<Symbol>,
    origin: Option<&SymbolOriginInfo>,
    completion_kind: CompletionKind,
    is_jsx_identifier_expected: bool,
) -> (String, bool) {
    if origin_is_ignore(origin) {
        return (String::new(), false);
    }

    let name = if origin_includes_symbol_name(origin) {
        origin.unwrap().symbol_name()
    } else {
        ast::symbol_name(symbol).to_string()
    };
    if name.is_empty()
        // If the symbol is external module, don't show it in the completion list
        // (i.e declare module "http" { const x; } | // <= request completion here, "http" should not be there)
        || symbol.flags().intersects(SymbolFlags::Module) && starts_with_quote(&name)
        // If the symbol is the internal name of an ES symbol, it is not a valid entry. Internal names for ES symbols start with "__@"
        || checker::is_known_symbol(symbol)
    {
        return (String::new(), false);
    }

    let variant =
        if is_jsx_identifier_expected { LanguageVariant::JSX } else { LanguageVariant::Standard };
    // name is a valid identifier or private identifier text
    if scanner::is_identifier_text(&name, variant)
        || symbol.value_declaration().is_some()
            && ast::is_private_identifier_class_element_declaration(
                symbol.value_declaration().unwrap(),
            )
    {
        return (name, false);
    }
    if symbol.flags().intersects(SymbolFlags::Alias) {
        // Allow non-identifier import/export aliases since we can insert them as string literals
        return (name, true);
    }

    match completion_kind {
        CompletionKind::MemberLike => {
            if origin_is_computed_property_name(origin) {
                return (origin.unwrap().symbol_name(), false);
            }
            (String::new(), false)
        }
        CompletionKind::ObjectPropertyDeclaration => (quote(file, preferences, &name), false),
        CompletionKind::PropertyAccess | CompletionKind::Global => {
            // For a 'this.' completion it will be in a global context, but may have a non-identifier name.
            // Don't add a completion for a name starting with a space. See https://github.com/Microsoft/TypeScript/pull/20547
            let ch = name.chars().next().unwrap_or('\u{FFFD}');
            if ch == ' ' {
                return (String::new(), false);
            }
            (name, true)
        }
        CompletionKind::None | CompletionKind::String => (name, false),
    }
}

// !!! refactor symbolOriginInfo so that we can tell the difference between flags and the kind of data it has
// completions.go:3220
fn origin_is_ignore(origin: Option<&SymbolOriginInfo>) -> bool {
    origin.is_some_and(|o| o.kind.intersects(symbolOriginInfoKind::Ignore))
}

// completions.go:3224
fn origin_includes_symbol_name(origin: Option<&SymbolOriginInfo>) -> bool {
    origin_is_computed_property_name(origin)
}

// completions.go:3228
fn origin_is_computed_property_name(origin: Option<&SymbolOriginInfo>) -> bool {
    origin.is_some_and(|o| o.kind.intersects(symbolOriginInfoKind::ComputedPropertyName))
}

// completions.go:3232
fn origin_is_object_literal_method(origin: Option<&SymbolOriginInfo>) -> bool {
    origin.is_some_and(|o| o.kind.intersects(symbolOriginInfoKind::ObjectLiteralMethod))
}

// completions.go:3236
fn origin_is_this_type_node(origin: Option<&SymbolOriginInfo>) -> bool {
    origin.is_some_and(|o| o.kind.intersects(symbolOriginInfoKind::ThisType))
}

// completions.go:3240
fn origin_is_type_only_alias(origin: Option<&SymbolOriginInfo>) -> bool {
    origin.is_some_and(|o| o.kind.intersects(symbolOriginInfoKind::TypeOnlyAlias))
}

// completions.go:3244
fn origin_is_symbol_member(origin: Option<&SymbolOriginInfo>) -> bool {
    origin.is_some_and(|o| o.kind.intersects(symbolOriginInfoKind::SymbolMember))
}

// completions.go:3248
fn origin_is_nullable_member(origin: Option<&SymbolOriginInfo>) -> bool {
    origin.is_some_and(|o| o.kind.intersects(symbolOriginInfoKind::Nullable))
}

// completions.go:3252
fn origin_is_promise(origin: Option<&SymbolOriginInfo>) -> bool {
    origin.is_some_and(|o| o.kind.intersects(symbolOriginInfoKind::Promise))
}

// completions.go:3256
pub(crate) fn get_source_from_origin(origin: Option<&SymbolOriginInfo>) -> &'static str {
    if origin_is_this_type_node(origin) {
        return COMPLETION_SOURCE_THIS_PROPERTY;
    }

    if origin_is_type_only_alias(origin) {
        return COMPLETION_SOURCE_TYPE_ONLY_ALIAS;
    }

    ""
}

// In a scenarion such as `const x = 1 * |`, the context and previous tokens are both `*`.
// In `const x = 1 * o|`, the context token is *, and the previous token is `o`.
// `contextToken` and `previousToken` can both be nil if we are at the beginning of the file.
// completions.go:3271
pub(crate) fn get_relevant_tokens(
    position: u32,
    file: P<SourceFile>,
) -> (Option<P<Node>>, Option<P<Node>>) {
    let previous_token = astnav::find_preceding_token(file, position);
    if let Some(previous_token) = previous_token
        && position <= previous_token.end().expect_source("parsed tokens have source positions")
        && (ast::is_member_name(previous_token) || ast::is_keyword_kind(previous_token.kind()))
    {
        let context_token = astnav::find_preceding_token(
            file,
            previous_token.pos().expect_source("parsed tokens have source positions"),
        );
        return (context_token, Some(previous_token));
    }
    (previous_token, previous_token)
}

// "." | '"' | "'" | "`" | "/" | "@" | "<" | "#" | " " | "*"
// completions.go:3281
pub(crate) type CompletionsTriggerCharacter = str;

// completions.go:3283
pub(crate) fn is_valid_trigger(
    file: P<SourceFile>,
    trigger_character: &CompletionsTriggerCharacter,
    context_token: Option<P<Node>>,
    position: u32,
) -> bool {
    match trigger_character {
        "." | "@" => true,
        "\"" | "'" | "`" => {
            // Only automatically bring up completions if this is an opening quote.
            context_token.is_some_and(|ct| {
                is_string_literal_or_template(ct)
                    && position == astnav::get_start_of_node(ct, file, false /*includeJSDoc*/) + 1
            })
        }
        "#" => context_token.is_some_and(|ct| {
            ast::is_private_identifier(ct) && ast::get_containing_class(ct).is_some()
        }),
        "<" => {
            // Opening JSX tag
            context_token.is_some_and(|ct| {
                ct.kind() == Kind::LessThanToken
                    && (!ast::is_binary_expression(ct.parent().unwrap())
                        || binary_expression_may_be_open_tag(ct.parent().unwrap()))
            })
        }
        "/" => {
            let Some(context_token) = context_token else {
                return false;
            };
            if ast::is_string_literal_like(context_token) {
                return ast::try_get_import_from_module_specifier(context_token).is_some();
            }
            context_token.kind() == Kind::LessThanSlashToken
                && ast::is_jsx_closing_element(context_token.parent().unwrap())
        }
        " " => context_token.is_some_and(|ct| {
            ct.kind() == Kind::ImportKeyword && ct.parent().unwrap().kind() == Kind::SourceFile
        }),
        "*" => crate::jsdoc_snippet::is_potentially_valid_jsdoc_snippet_completion_position(
            file, position,
        ),
        _ => panic!("Unknown trigger character: {trigger_character}"),
    }
}

// completions.go:3318
fn is_string_literal_or_template(node: P<Node>) -> bool {
    matches!(
        node.kind(),
        Kind::StringLiteral
            | Kind::NoSubstitutionTemplateLiteral
            | Kind::TemplateExpression
            | Kind::TaggedTemplateExpression
    )
}

// completions.go:3327
pub(crate) fn binary_expression_may_be_open_tag(binary_expression: P<Node>) -> bool {
    ast::node_is_missing(binary_expression.as_binary_expression().left)
}

// completions.go:3331
pub(crate) fn is_checked_file(file: P<SourceFile>, compiler_options: &CompilerOptions) -> bool {
    !ast::is_source_file_js(file) || ast::is_check_js_enabled_for_file(file, compiler_options)
}

// completions.go:3335
pub(crate) fn is_context_token_value_location(context_token: Option<P<Node>>) -> bool {
    context_token.is_some_and(|context_token| {
        let parent = context_token.parent().unwrap();
        (context_token.kind() == Kind::TypeOfKeyword
            && (parent.kind() == Kind::TypeQuery || ast::is_type_of_expression(parent)))
            || (context_token.kind() == Kind::AssertsKeyword
                && parent.kind() == Kind::TypePredicate)
    })
}

// completions.go:3341
pub(crate) fn is_possibly_type_argument_position(
    token: Option<P<Node>>,
    source_file: P<SourceFile>,
    type_checker: &mut Checker,
) -> bool {
    let Some(info) = get_possible_type_arguments_info(token, source_file) else {
        return false;
    };
    ast::is_part_of_type_node(info.called)
        || !get_possible_generic_signatures(info.called, info.n_type_arguments, type_checker)
            .is_empty()
        || is_possibly_type_argument_position(Some(info.called), source_file, type_checker)
}

// completions.go:3348
pub(crate) fn is_context_token_type_location(context_token: Option<P<Node>>) -> bool {
    if let Some(context_token) = context_token {
        let parent_kind = context_token.parent().unwrap().kind();
        match context_token.kind() {
            Kind::ColonToken => {
                return parent_kind == Kind::PropertyDeclaration
                    || parent_kind == Kind::PropertySignature
                    || parent_kind == Kind::Parameter
                    || parent_kind == Kind::VariableDeclaration
                    || ast::is_function_like_kind(parent_kind);
            }
            Kind::EqualsToken => {
                return parent_kind == Kind::TypeAliasDeclaration
                    || parent_kind == Kind::TypeParameter;
            }
            Kind::AsKeyword => return parent_kind == Kind::AsExpression,
            Kind::LessThanToken => {
                return parent_kind == Kind::TypeReference
                    || parent_kind == Kind::TypeAssertionExpression;
            }
            Kind::ExtendsKeyword => return parent_kind == Kind::TypeParameter,
            Kind::SatisfiesKeyword => return parent_kind == Kind::SatisfiesExpression,
            Kind::OpenBracketToken | Kind::CommaToken => return parent_kind == Kind::TupleType,
            _ => {}
        }
    }
    false
}

// Go passes `collections.Set[ast.SymbolId]` by value: copies share the map once it exists, and adding to a set
// whose map is still nil allocates a map that only that copy sees.
pub(crate) type SeenModules = Option<Rc<RefCell<FxHashSet<SymbolId>>>>;

fn seen_modules_add_if_absent(s: &mut SeenModules, id: SymbolId) -> bool {
    s.get_or_insert_with(Default::default).borrow_mut().insert(id)
}

// True if symbol is a type or a module containing at least one type.
// completions.go:3376
pub(crate) fn symbol_can_be_referenced_at_type_location(
    symbol: P<Symbol>,
    type_checker: &mut Checker,
    seen_modules: SeenModules,
) -> bool {
    // Since an alias can be merged with a local declaration, we need to test both the alias and its target.
    // This code used to just test the result of `skipAlias`, but that would ignore any locally introduced meanings.
    if non_alias_can_be_referenced_at_type_location(symbol, type_checker, seen_modules.clone()) {
        return true;
    }
    let target = checker::skip_alias(symbol.export_symbol().unwrap_or(symbol), type_checker);
    non_alias_can_be_referenced_at_type_location(target, type_checker, seen_modules)
}

// completions.go:3387
fn non_alias_can_be_referenced_at_type_location(
    symbol: P<Symbol>,
    type_checker: &mut Checker,
    mut seen_modules: SeenModules,
) -> bool {
    if symbol.flags().intersects(SymbolFlags::Type) || type_checker.is_unknown_symbol(symbol) {
        return true;
    }
    if symbol.flags().intersects(SymbolFlags::Module)
        && seen_modules_add_if_absent(&mut seen_modules, ast::get_symbol_id(symbol))
        && type_checker.get_exports_of_module_exported(symbol).into_iter().any(|e| {
            symbol_can_be_referenced_at_type_location(e, type_checker, seen_modules.clone())
        })
    {
        return true;
    }
    false
}

// Gets all properties on a type, but if that type is a union of several types,
// excludes array-like types or callable/constructable types.
// completions.go:3398
pub(crate) fn get_properties_for_completion(
    t: P<Type>,
    type_checker: &mut Checker,
) -> Vec<P<Symbol>> {
    if t.is_union() {
        type_checker.get_all_possible_properties_of_types(t.types())
    } else {
        type_checker.get_apparent_properties(t)
    }
}

// Given 'a.b.c', returns 'a'.
// completions.go:3407
pub(crate) fn get_left_most_name(e: P<Node>) -> Option<P<Node>> {
    if ast::is_identifier(e) {
        Some(e)
    } else if ast::is_property_access_expression(e) {
        get_left_most_name(e.expression().unwrap())
    } else {
        None
    }
}

// completions.go:3417
pub(crate) fn get_first_symbol_in_chain(
    symbol: P<Symbol>,
    enclosing_declaration: Option<P<Node>>,
    type_checker: &mut Checker,
) -> Option<P<Symbol>> {
    let chain = type_checker.get_accessible_symbol_chain_exported(
        symbol,
        enclosing_declaration,
        SymbolFlags::All, /*meaning*/
        false,            /*useOnlyExternalAliasing*/
    );
    if !chain.is_empty() {
        return Some(chain[0]);
    }
    if let Some(parent) = symbol.parent() {
        if is_module_symbol(parent) {
            return Some(symbol);
        }
        return get_first_symbol_in_chain(parent, enclosing_declaration, type_checker);
    }
    None
}

// completions.go:3436
fn is_module_symbol(symbol: P<Symbol>) -> bool {
    symbol.declarations().iter().any(|decl| decl.kind() == Kind::SourceFile)
}

// completions.go:3440
pub(crate) fn get_nullable_symbol_origin_info_kind(
    kind: symbolOriginInfoKind,
    insert_question_dot: bool,
) -> symbolOriginInfoKind {
    let mut kind = kind;
    if insert_question_dot {
        kind |= symbolOriginInfoKind::Nullable;
    }
    kind
}

// completions.go:3447
pub(crate) fn is_static_property(symbol: P<Symbol>) -> bool {
    symbol.value_declaration().is_some_and(|vd| {
        vd.modifier_flags().intersects(ModifierFlags::Static)
            && ast::is_class_like(vd.parent().unwrap())
    })
}

// getContextualTypeForConditionalExpression handles completion within a conditional expression
// (ternary operator) by using the parent expression to find the contextual type.
// completions.go:3455
pub(crate) fn get_contextual_type_for_conditional_expression(
    conditional_expr: P<Node>,
    position: u32,
    file: P<SourceFile>,
    type_checker: &mut Checker,
) -> Option<P<Type>> {
    let arg_info =
        get_argument_info_for_completions(conditional_expr, position, file, type_checker);
    if let Some(arg_info) = arg_info {
        return Some(type_checker.get_contextual_type_for_argument_at_index_exported(
            arg_info.invocation,
            arg_info.argument_index,
        ));
    }
    // Fall through to regular contextual type logic if not in an argument
    let contextual_type = type_checker
        .get_contextual_type_exported(conditional_expr, ContextFlags::IgnoreNodeInferences);
    if contextual_type.is_some() {
        return contextual_type;
    }
    type_checker.get_contextual_type_exported(conditional_expr, ContextFlags::None)
}
