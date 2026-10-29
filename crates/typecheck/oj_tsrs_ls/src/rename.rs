use std::collections::hash_map::Entry;

use rustc_hash::FxHashMap;
use tsrs_ast::{self as ast, Kind, Node, SourceFile, Symbol, SymbolFlags};
use tsrs_checker::{Checker, Type};
use tsrs_compiler::Program;
use tsrs_core::collections::OrderedMap;
use tsrs_core::context::Context;
use tsrs_core::{P, TextRange, Tristate, tspath};
use tsrs_diagnostics::{self as diagnostics, Message};
use tsrs_lsproto as lsproto;

use crate::astnav;
use crate::crossproject::{CrossProjectOrchestrator, combine_rename_response};
use crate::findallreferences::{
    EntryKind, ReferenceEntry, SymbolAndEntriesData, SymbolEntryTransformOptions,
};
use crate::languageservice::LanguageService;
use crate::lsconv::{Converters, Script};
use crate::lsutil::{self, QuotePreference, UserPreferences};
use crate::spanmap::Feature;
use crate::symbols::is_inside_node_modules;
use crate::utilities::{
    get_contextual_type_from_parent_or_ancestor_type_node,
    is_literal_name_of_property_declaration_or_index_access,
    is_object_binding_element_without_property_name,
};

// RenameInfo represents the result of a rename validation check.
// It is used by the `textDocument/prepareRename` LSP handler.
// rename.go:25
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RenameInfo {
    pub can_rename: bool,
    pub localized_error_message: String,
    pub display_name: String,
    pub trigger_span: lsproto::Range,
    pub file_to_rename: String,
    pub new_file_name: String,
}

// rename.go:34
struct MappedRenameEdit {
    uri: lsproto::DocumentUri,
    edit: lsproto::TextEdit,
}

// rename.go:39
#[derive(Clone, PartialEq, Eq, Hash)]
struct RenameEditKey {
    uri: lsproto::DocumentUri,
    text_range: lsproto::Range,
}

// Go returns `(map, ok)`; the map's iteration order is random in Go, the port keeps first-seen order.
// rename.go:44
fn deduplicate_rename_edits(
    mapped_edits: Vec<MappedRenameEdit>,
) -> Option<OrderedMap<lsproto::DocumentUri, Vec<lsproto::TextEdit>>> {
    let mut edit_texts: FxHashMap<RenameEditKey, String> = FxHashMap::default();
    let mut unique_edits: Vec<MappedRenameEdit> = Vec::with_capacity(mapped_edits.len());
    for mapped_edit in mapped_edits {
        let key =
            RenameEditKey { uri: mapped_edit.uri.clone(), text_range: mapped_edit.edit.range };
        match edit_texts.entry(key) {
            Entry::Occupied(entry) => {
                if *entry.get() != mapped_edit.edit.new_text {
                    return None;
                }
                continue;
            }
            Entry::Vacant(entry) => {
                entry.insert(mapped_edit.edit.new_text.clone());
            }
        }
        unique_edits.push(mapped_edit);
    }
    let mut changes: OrderedMap<lsproto::DocumentUri, Vec<lsproto::TextEdit>> =
        OrderedMap::default();
    for mapped_edit in unique_edits {
        changes.entry(mapped_edit.uri).or_default().push(mapped_edit.edit);
    }
    Some(changes)
}

impl LanguageService {
    // rename.go:65
    pub fn provide_rename(
        &self,
        ctx: &Context,
        params: &lsproto::RenameParams,
        orchestrator: Option<&dyn CrossProjectOrchestrator>,
    ) -> Result<lsproto::WorkspaceEditOrNull, lsproto::Error> {
        self.handle_cross_project(
            ctx,
            params,
            orchestrator,
            LanguageService::symbol_and_entries_to_rename,
            combine_rename_response,
            true,  /*isRename*/
            false, /*implementations*/
            SymbolEntryTransformOptions::default(),
            None, /*defaultProjectData*/
        )
    }

    // rename.go:79
    pub fn get_rename_info(
        &self,
        ctx: &Context,
        new_name: &str,
        document_uri: &lsproto::DocumentUri,
        position: lsproto::Position,
    ) -> RenameInfo {
        let (program, source_file) = self.get_program_and_file(document_uri);
        let positions = self.converters.from_lsp_position_for_source_file(
            source_file,
            position,
            Feature::Rename,
        );
        if let Some(result) = positions.into_iter().find_map(|mapped| {
            if !mapped.fidelity.is_exact() {
                return None;
            }
            let source_file = mapped.script;
            let mut node = astnav::get_touching_property_name(source_file, mapped.position);
            node = crate::utilities::get_adjusted_location(
                node,
                true, /*forRename*/
                Some(source_file),
            );
            if node_is_eligible_for_rename(Some(node))
                && let Some(rename_info) =
                    self.get_rename_info_for_node(ctx, new_name, node, source_file, program)
            {
                return Some(rename_info);
            }

            None
        }) {
            return result;
        }
        get_rename_info_error(ctx, &diagnostics::YOU_CANNOT_RENAME_THIS_ELEMENT)
    }

    // rename.go:98
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Language service operations share the fallible response signature used by LSP and cross-project dispatch."
    )]
    fn symbol_and_entries_to_rename(
        &self,
        ctx: &Context,
        params: &lsproto::RenameParams,
        data: &SymbolAndEntriesData,
        _options: SymbolEntryTransformOptions,
    ) -> Result<lsproto::WorkspaceEditOrNull, lsproto::Error> {
        if !node_is_eligible_for_rename(data.original_node) {
            return Ok(lsproto::WorkspaceEditOrNull::default());
        }
        let original_node = data.original_node.unwrap();

        let program = self.get_program();

        // Defense-in-depth: validate rename eligibility even if the client skipped prepareRename.
        // Use getRenameInfoForNode directly with the already-resolved node to avoid
        // re-resolving the position and polluting state baselines.
        let source_file = ast::get_source_file_of_node(original_node).unwrap();
        match self.get_rename_info_for_node(
            ctx,
            &params.new_name,
            original_node,
            source_file,
            program,
        ) {
            Some(info) if info.can_rename => {}
            _ => return Ok(lsproto::WorkspaceEditOrNull::default()),
        }

        let entries: Vec<_> =
            data.symbols_and_entries.iter().flat_map(|s| s.references.iter().cloned()).collect();
        let mut mapped_edits: Vec<MappedRenameEdit> = Vec::new();
        let mut ch = program.get_type_checker(ctx);
        let ch: &mut Checker = &mut ch;

        let quote_preference = lsutil::get_quote_preference(source_file, self.user_preferences());
        let use_aliases_for_rename =
            self.user_preferences().use_aliases_for_rename.is_true_or_unknown();

        mapped_edits.extend(entries.iter().filter_map(|entry| {
            let uri = self.get_file_name_of_entry(entry);
            if self.user_preferences().allow_rename_of_import_path != Tristate::True
                && entry.node.is_some_and(|node| {
                    ast::is_string_literal_like(node)
                        && ast::try_get_import_from_module_specifier(node).is_some()
                })
            {
                return None;
            }
            let Some(rng) = self.rename_edit_range(entry) else {
                // The occurrence lies outside a verbatim span of a content-mapped file, so it cannot be
                // written back to the original text. Skip it and keep renaming the remaining occurrences.
                return None;
            };
            let text_edit = lsproto::TextEdit {
                range: rng,
                new_text: Self::get_text_for_rename(
                    original_node,
                    entry,
                    &params.new_name,
                    ch,
                    quote_preference,
                    use_aliases_for_rename,
                ),
            };
            Some(MappedRenameEdit { uri, edit: text_edit })
        }));
        let Some(changes) = deduplicate_rename_edits(mapped_edits) else {
            return Ok(lsproto::WorkspaceEditOrNull::default());
        };
        Ok(lsproto::WorkspaceEditOrNull {
            workspace_edit: Some(lsproto::WorkspaceEdit {
                changes: Some(changes),
                ..Default::default()
            }),
        })
    }

    // renameEditRange returns the LSP range at which a rename occurrence should be edited. For occurrences in
    // content-mapped files it maps the transformed range strictly, returning ok=false when the occurrence is
    // not fully within a single verbatim span, so the caller can skip an edit that cannot be applied to the
    // original text.
    // rename.go:153
    fn rename_edit_range(&self, entry: &ReferenceEntry) -> Option<lsproto::Range> {
        self.resolve_entry(entry);
        let Some(node) = entry.node else {
            let (location, fidelity) = self.source_file_range_to_lsp_location(
                entry.source_file.get().unwrap(),
                entry.text_range.get().unwrap(),
            );
            return if fidelity.is_exact() { Some(location.range) } else { None };
        };
        let source_file = ast::get_source_file_of_node(node);
        let Some(source_file) = source_file.filter(|f| Script::span_map(f).is_some()) else {
            return Some(self.get_range_of_entry(entry));
        };
        let (lsp_range, fidelity) =
            self.converters.to_lsp_range(&source_file, entry.text_range.get().unwrap());
        if fidelity.is_exact() { Some(lsp_range) } else { None }
    }

    // getRenameInfoForNode performs detailed validation for a rename operation on a specific node.
    // Go returns `(info, ok)`.
    // rename.go:168
    fn get_rename_info_for_node(
        &self,
        ctx: &Context,
        new_name: &str,
        node: P<Node>,
        source_file: P<SourceFile>,
        program: &'static Program,
    ) -> Option<RenameInfo> {
        let mut ch = program.get_type_checker(ctx);
        let ch: &mut Checker = &mut ch;

        let symbol = ch.get_symbol_at_location_exported(node);
        let Some(symbol) = symbol else {
            if ast::is_string_literal_like(node) {
                // Allow renaming of string literal types with contextual string literal types
                let typ = get_contextual_type_from_parent_or_ancestor_type_node(node, ch);
                if let Some(typ) = typ
                    && (typ.is_string_literal()
                        || (typ.is_union()
                            && typ.types().iter().all(|t: &P<Type>| t.is_string_literal())))
                {
                    return Some(get_rename_info_success(
                        node,
                        source_file,
                        node.text(),
                        &self.converters,
                    ));
                }
            } else if ast::is_label_name(node) {
                let name = node.text();
                return Some(get_rename_info_success(node, source_file, name, &self.converters));
            }
            return None;
        };

        // Only allow a symbol to be renamed if it actually has at least one declaration.
        if symbol.declarations().is_empty() {
            return None;
        }

        if let Some(msg) = self.rename_blocked_reason(source_file, node, symbol, ch, program) {
            return Some(get_rename_info_error(ctx, msg));
        }

        if ast::is_string_literal_like(node)
            && ast::try_get_import_from_module_specifier(node).is_some()
        {
            if self.user_preferences().allow_rename_of_import_path.is_true() {
                return self.get_rename_info_for_module(ctx, new_name, node, source_file, symbol);
            }
            return None;
        }

        Some(get_rename_info_success(
            node,
            source_file,
            &ch.symbol_to_string_exported(symbol),
            &self.converters,
        ))
    }
}

// rename.go:209
pub fn node_is_eligible_for_rename(node: Option<P<Node>>) -> bool {
    let Some(node) = node else {
        return false;
    };
    match node.kind() {
        Kind::Identifier
        | Kind::PrivateIdentifier
        | Kind::StringLiteral
        | Kind::NoSubstitutionTemplateLiteral
        | Kind::ThisKeyword => true,
        Kind::NumericLiteral => is_literal_name_of_property_declaration_or_index_access(node),
        _ => false,
    }
}

impl LanguageService {
    // renameBlockedReason returns a non-nil diagnostic message if the rename should be blocked
    // because the symbol is a library definition, a default keyword, or would cross node_modules boundaries.
    // rename.go:229
    fn rename_blocked_reason(
        &self,
        source_file: P<SourceFile>,
        node: P<Node>,
        symbol: P<Symbol>,
        ch: &mut Checker,
        program: &'static Program,
    ) -> Option<&'static Message> {
        if let Some(result) = symbol.declarations().iter().find_map(|&declaration| {
            if is_defined_in_library_file(program, declaration) {
                Some(Some(&diagnostics::YOU_CANNOT_RENAME_ELEMENTS_THAT_ARE_DEFINED_IN_THE_STANDARD_TYPE_SCRIPT_LIBRARY))
            } else {
                None
            }
        }) {
            return result;
        }

        // Cannot rename `default` as in `import { default as foo } from "./someModule"`
        if ast::is_identifier(node)
            && node.text() == "default"
            && symbol.parent().is_some_and(|parent| parent.flags().intersects(SymbolFlags::Module))
        {
            return Some(&diagnostics::YOU_CANNOT_RENAME_THIS_ELEMENT);
        }

        if let Some(msg) =
            would_rename_in_other_node_modules(source_file, symbol, ch, self.user_preferences())
        {
            return Some(msg);
        }

        None
    }
}

// isDefinedInLibraryFile checks if a declaration is from a default library file (e.g., lib.d.ts).
// rename.go:249
fn is_defined_in_library_file(program: &'static Program, declaration: P<Node>) -> bool {
    let decl_source_file = ast::get_source_file_of_node(declaration).unwrap();
    program.is_source_file_default_library(decl_source_file.path())
        && tspath::is_declaration_file_name(decl_source_file.file_name())
}

// wouldRenameInOtherNodeModules checks if renaming the symbol would affect node_modules.
// rename.go:255
fn would_rename_in_other_node_modules(
    original_file: P<SourceFile>,
    symbol: P<Symbol>,
    ch: &mut Checker,
    preferences: &UserPreferences,
) -> Option<&'static Message> {
    let mut sym = symbol;
    if !preferences.use_aliases_for_rename.is_true_or_unknown()
        && sym.flags().intersects(SymbolFlags::Alias)
    {
        let import_specifier =
            sym.declarations().iter().copied().find(|&d| ast::is_import_specifier(d));
        if let Some(import_specifier) = import_specifier
            && import_specifier.as_import_specifier().property_name().is_none()
        {
            sym = ch.get_aliased_symbol(sym);
        }
    }

    let declarations = sym.declarations();
    if declarations.is_empty() {
        return None;
    }

    let original_package =
        tsrs_module::node_module_package_root_for_file(original_file.file_name());
    if original_package.is_empty() {
        // Original source file is not in node_modules.
        if let Some(result) = declarations.iter().find_map(|&declaration| {
            if is_inside_node_modules(ast::get_source_file_of_node(declaration).unwrap().file_name()) {
                Some(Some(&diagnostics::YOU_CANNOT_RENAME_ELEMENTS_THAT_ARE_DEFINED_IN_A_NODE_MODULES_FOLDER))
            } else {
                None
            }
        }) {
            return result;
        }
        return None;
    }

    // Original source file is in node_modules.
    if let Some(result) = declarations.iter().find_map(|&declaration| {
        let decl_package = tsrs_module::node_module_package_root_for_file(ast::get_source_file_of_node(declaration).unwrap().file_name());
        if !decl_package.is_empty() && decl_package != original_package {
            return Some(Some(&diagnostics::YOU_CANNOT_RENAME_ELEMENTS_THAT_ARE_DEFINED_IN_ANOTHER_NODE_MODULES_FOLDER));
        }

        None
    }) {
        return result;
    }
    None
}

// rename.go:290
pub fn client_supports_will_rename_files(ctx: &Context) -> bool {
    lsproto::get_client_capabilities(ctx).workspace.file_operations.will_rename
}

// rename.go:294
pub fn client_supports_document_changes(ctx: &Context) -> bool {
    lsproto::get_client_capabilities(ctx).workspace.workspace_edit.document_changes
}

// rename.go:298
pub fn client_supports_rename_resource_operations(ctx: &Context) -> bool {
    lsproto::get_client_capabilities(ctx)
        .workspace
        .workspace_edit
        .resource_operations
        .contains(&lsproto::ResourceOperationKind::RENAME)
}

impl LanguageService {
    // getRenameInfoForModule handles rename validation for module specifiers.
    // rename.go:303
    fn get_rename_info_for_module(
        &self,
        ctx: &Context,
        new_name: &str,
        specifier: P<Node>, /*StringLiteralLike*/
        source_file: P<SourceFile>,
        module_symbol: P<Symbol>,
    ) -> Option<RenameInfo> {
        if !tspath::is_external_module_name_relative(specifier.text()) {
            return Some(get_rename_info_error(
                ctx,
                &diagnostics::YOU_CANNOT_RENAME_A_MODULE_VIA_A_GLOBAL_IMPORT,
            ));
        }
        if !client_supports_document_changes(ctx)
            || !client_supports_rename_resource_operations(ctx)
        {
            return Some(get_rename_info_error(
                ctx,
                &diagnostics::FILE_RENAME_IS_NOT_SUPPORTED_BY_THE_EDITOR,
            ));
        }

        let module_source_file =
            module_symbol.declarations().iter().copied().find(|&d| ast::is_source_file(d))?;

        let file_name = module_source_file.as_source_file().file_name();
        let mut without_index = String::new();
        let specifier_text = specifier.text();
        if !specifier_text.ends_with("/index") && !specifier_text.ends_with("/index.js") {
            let candidate = tspath::remove_file_extension(file_name);
            if let Some(trimmed) = candidate.strip_suffix("/index") {
                without_index = trimmed.to_string();
            }
        }

        let mut display_name = file_name.to_string();
        if !without_index.is_empty() {
            display_name = without_index;
        }
        let new_file_name =
            self.get_new_file_name_for_module_rename(&display_name, specifier_text, new_name);

        // Span should only be the last component of the path. + 1 to account for the quote character.
        let index_after_last_slash = specifier_text.rfind('/').map_or(0, |i| i + 1);
        let start = astnav::get_start_of_node(specifier, source_file, false /*includeJSDoc*/)
            + 1
            + u32::try_from(index_after_last_slash).expect("module specifier offset exceeds u32");
        let length = u32::try_from(specifier_text.len() - index_after_last_slash)
            .expect("module specifier length exceeds u32");

        let (trigger_span, fidelity) =
            self.converters.to_lsp_range(&source_file, TextRange::new(start, start + length));
        if !fidelity.is_exact() {
            return None;
        }
        Some(RenameInfo {
            can_rename: true,
            display_name: specifier_text[index_after_last_slash..].to_string(),
            trigger_span,
            file_to_rename: display_name,
            new_file_name,
            ..Default::default()
        })
    }

    // Adjust the new name based on the old path that an import specifier resolves to.
    // For example, if specifier "a.js" resolves to file a.ts, renaming "a.js" -> "b.js" should mean file rename a.ts -> b.ts.
    // rename.go:351
    fn get_new_file_name_for_module_rename(
        &self,
        old_path: &str,
        specifier_text: &str,
        new_name: &str,
    ) -> String {
        let mut new_path =
            tspath::combine_paths(&tspath::get_directory_path(old_path), &[new_name]);
        let ignore_case = !self.use_case_sensitive_file_names();
        let old_ext = if tspath::is_declaration_file_name(old_path) {
            tspath::get_declaration_file_extension(old_path).to_string()
        } else {
            tspath::get_any_extension_from_path(old_path, &[] /*extensions*/, ignore_case)
        };
        if !tspath::has_extension(&new_path) {
            new_path.push_str(&old_ext);
        } else if tspath::get_any_extension_from_path(
            &new_path,
            &[], /*extensions*/
            ignore_case,
        ) == tspath::get_any_extension_from_path(
            specifier_text,
            &[], /*extensions*/
            ignore_case,
        ) {
            new_path = tspath::change_any_extension(
                &new_path,
                &old_ext,
                &[], /*extensions*/
                ignore_case,
            );
        }
        new_path
    }

    // rename.go:368
    fn get_text_for_rename(
        original_node: P<Node>,
        entry: &ReferenceEntry,
        new_text: &str,
        ch: &mut Checker,
        quote_preference: QuotePreference,
        use_aliases_for_rename: bool,
    ) -> String {
        if use_aliases_for_rename
            && entry.kind != EntryKind::Range
            && (ast::is_identifier(original_node) || ast::is_string_literal_like(original_node))
        {
            let node = ast::get_reparsed_node_for_node(entry.node).unwrap();
            let kind = entry.kind;
            let parent = node.parent().unwrap();
            let name = original_node.text();
            let is_shorthand_assignment = ast::is_shorthand_property_assignment(parent);
            if is_shorthand_assignment
                || (is_object_binding_element_without_property_name(parent)
                    && parent.name() == Some(node)
                    && parent.as_binding_element().dot_dot_dot_token().is_none())
            {
                if kind == EntryKind::SearchedLocalFoundProperty {
                    return format!("{name}: {new_text}");
                }
                if kind == EntryKind::SearchedPropertyFoundLocal {
                    return format!("{new_text}: {name}");
                }
                // In `const o = { x }; o.x`, symbolAtLocation at `x` in `{ x }` is the property symbol.
                // For a binding element `const { x } = o;`, symbolAtLocation at `x` is the property symbol.
                if is_shorthand_assignment {
                    let grand_parent = parent.parent().unwrap();
                    if ast::is_object_literal_expression(grand_parent)
                        && ast::is_binary_expression(grand_parent.parent().unwrap())
                        && ast::is_module_exports_access_expression(
                            grand_parent.parent().unwrap().as_binary_expression().left,
                        )
                    {
                        return format!("{name}: {new_text}");
                    }
                    return format!("{new_text}: {name}");
                }
                return format!("{name}: {new_text}");
            } else if ast::is_import_specifier(parent) && parent.property_name().is_none() {
                // If the original symbol was using this alias, just rename the alias.
                let original_symbol = if ast::is_export_specifier(original_node.parent().unwrap()) {
                    ch.get_export_specifier_local_target_symbol(original_node.parent().unwrap())
                } else {
                    ch.get_symbol_at_location_exported(original_node)
                };
                if original_symbol.is_some_and(|s| s.declarations().contains(&parent)) {
                    return format!("{name} as {new_text}");
                }
                return new_text.to_string();
            } else if ast::is_export_specifier(parent) && parent.property_name().is_none() {
                // If the symbol for the node is same as declared node symbol use prefix text
                if Some(original_node) == entry.node
                    || ch.get_symbol_at_location_exported(original_node)
                        == ch.get_symbol_at_location_exported(entry.node.unwrap())
                {
                    return format!("{name} as {new_text}");
                }
                return format!("{new_text} as {name}");
            }
        }

        // If the node is a numerical indexing literal, then add quotes around the property access.
        if entry.kind != EntryKind::Range
            && ast::is_numeric_literal(entry.node.unwrap())
            && ast::is_access_expression(entry.node.unwrap().parent().unwrap())
        {
            let quote = get_quote_from_preference(quote_preference);
            return format!("{quote}{new_text}{quote}");
        }

        new_text.to_string()
    }
}

// rename.go:423
fn get_quote_from_preference(quote_preference: QuotePreference) -> &'static str {
    if quote_preference == QuotePreference::Single {
        return "'";
    }
    "\""
}

// Go localizes with `locale.FromContext(ctx)`; only English is ported.
// rename.go:430
fn get_rename_info_error(_ctx: &Context, message: &Message) -> RenameInfo {
    RenameInfo {
        can_rename: false,
        localized_error_message: message.localize(&[]),
        ..Default::default()
    }
}

// rename.go:437
fn get_rename_info_success(
    node: P<Node>,
    source_file: P<SourceFile>,
    display_name: &str,
    converters: &Converters,
) -> RenameInfo {
    let mut start = astnav::get_start_of_node(node, source_file, false /*includeJSDoc*/);
    let mut end = node.end().expect_source("parsed rename targets have source positions");
    if ast::is_string_literal_like(node) {
        // Exclude the quotes
        start += 1;
        end -= 1;
    }
    let (trigger_span, fidelity) =
        converters.to_lsp_range(&source_file, TextRange::new(start, end));
    if !fidelity.is_exact() {
        return RenameInfo { can_rename: false, ..Default::default() };
    }
    RenameInfo {
        can_rename: true,
        display_name: display_name.to_string(),
        trigger_span,
        ..Default::default()
    }
}
