use std::sync::Arc;

use tsrs_ast::{self as ast, Diagnostic, Node, SourceFile, Symbol, SymbolFlags};
use tsrs_checker::Checker;
use tsrs_core::context::Context;
use tsrs_core::{CompilerOptions, JsxEmit, P, tspath};
use tsrs_diagnostics as diagnostics;
use tsrs_lsproto as lsproto;
use tsrs_scanner as scanner;

use crate::astnav;
use crate::autoimport::{self, ImportAdder, QueryKind, View};
use crate::codeactions::{
    CodeAction, CodeFixContext, CodeFixProvider, CombinedCodeActions, is_fixable_diagnostic,
};

// codeactions_importfixes.go:20
fn import_fix_error_codes() -> &'static [i32] {
    static CODES: std::sync::LazyLock<Vec<i32>> = std::sync::LazyLock::new(|| {
        vec![
            diagnostics::CANNOT_FIND_NAME_0.code(),
            diagnostics::CANNOT_FIND_NAME_0_DID_YOU_MEAN_1.code(),
            diagnostics::CANNOT_FIND_NAME_0_DID_YOU_MEAN_THE_INSTANCE_MEMBER_THIS_0.code(),
            diagnostics::CANNOT_FIND_NAME_0_DID_YOU_MEAN_THE_STATIC_MEMBER_1_0.code(),
            diagnostics::CANNOT_FIND_NAMESPACE_0.code(),
            diagnostics::X_0_REFERS_TO_A_UMD_GLOBAL_BUT_THE_CURRENT_FILE_IS_A_MODULE_CONSIDER_ADDING_AN_IMPORT_INSTEAD.code(),
            diagnostics::X_0_ONLY_REFERS_TO_A_TYPE_BUT_IS_BEING_USED_AS_A_VALUE_HERE.code(),
            diagnostics::NO_VALUE_EXISTS_IN_SCOPE_FOR_THE_SHORTHAND_PROPERTY_0_EITHER_DECLARE_ONE_OR_PROVIDE_AN_INITIALIZER.code(),
            diagnostics::X_0_CANNOT_BE_USED_AS_A_VALUE_BECAUSE_IT_WAS_IMPORTED_USING_IMPORT_TYPE.code(),
            diagnostics::CANNOT_FIND_NAME_0_DO_YOU_NEED_TO_INSTALL_TYPE_DEFINITIONS_FOR_J_QUERY_TRY_NPM_I_SAVE_DEV_TYPES_SLASHJQUERY.code(),
            diagnostics::CANNOT_FIND_NAME_0_DO_YOU_NEED_TO_CHANGE_YOUR_TARGET_LIBRARY_TRY_CHANGING_THE_LIB_COMPILER_OPTION_TO_1_OR_LATER.code(),
            diagnostics::CANNOT_FIND_NAME_0_DO_YOU_NEED_TO_CHANGE_YOUR_TARGET_LIBRARY_TRY_CHANGING_THE_LIB_COMPILER_OPTION_TO_INCLUDE_DOM.code(),
            diagnostics::CANNOT_FIND_NAME_0_DO_YOU_NEED_TO_INSTALL_TYPE_DEFINITIONS_FOR_A_TEST_RUNNER_TRY_NPM_I_SAVE_DEV_TYPES_SLASHJEST_OR_NPM_I_SAVE_DEV_TYPES_SLASHMOCHA_AND_THEN_ADD_JEST_OR_MOCHA_TO_THE_TYPES_FIELD_IN_YOUR_TSCONFIG.code(),
            diagnostics::CANNOT_FIND_NAME_0_DID_YOU_MEAN_TO_WRITE_THIS_IN_AN_ASYNC_FUNCTION.code(),
            diagnostics::CANNOT_FIND_NAME_0_DO_YOU_NEED_TO_INSTALL_TYPE_DEFINITIONS_FOR_J_QUERY_TRY_NPM_I_SAVE_DEV_TYPES_SLASHJQUERY_AND_THEN_ADD_JQUERY_TO_THE_TYPES_FIELD_IN_YOUR_TSCONFIG.code(),
            diagnostics::CANNOT_FIND_NAME_0_DO_YOU_NEED_TO_INSTALL_TYPE_DEFINITIONS_FOR_A_TEST_RUNNER_TRY_NPM_I_SAVE_DEV_TYPES_SLASHJEST_OR_NPM_I_SAVE_DEV_TYPES_SLASHMOCHA.code(),
            diagnostics::CANNOT_FIND_NAME_0_DO_YOU_NEED_TO_INSTALL_TYPE_DEFINITIONS_FOR_NODE_TRY_NPM_I_SAVE_DEV_TYPES_SLASHNODE.code(),
            diagnostics::CANNOT_FIND_NAME_0_DO_YOU_NEED_TO_INSTALL_TYPE_DEFINITIONS_FOR_NODE_TRY_NPM_I_SAVE_DEV_TYPES_SLASHNODE_AND_THEN_ADD_NODE_TO_THE_TYPES_FIELD_IN_YOUR_TSCONFIG.code(),
            diagnostics::CANNOT_FIND_NAMESPACE_0_DID_YOU_MEAN_1.code(),
            diagnostics::CANNOT_EXTEND_AN_INTERFACE_0_DID_YOU_MEAN_IMPLEMENTS.code(),
            diagnostics::THIS_JSX_TAG_REQUIRES_0_TO_BE_IN_SCOPE_BUT_IT_COULD_NOT_BE_FOUND.code(),
        ]
    });
    &CODES
}

// codeactions_importfixes.go:44
const IMPORT_FIX_ID: &str = "fixMissingImport";

// codeactions_importfixes.go:48
// ImportFixProvider is the CodeFixProvider for import-related fixes
pub static IMPORT_FIX_PROVIDER: std::sync::LazyLock<CodeFixProvider> =
    std::sync::LazyLock::new(|| CodeFixProvider {
        error_codes: import_fix_error_codes(),
        get_code_actions: get_import_code_actions,
        get_all_code_actions: Some(get_all_import_code_actions),
    });

// codeactions_importfixes.go:55
struct FixInfo {
    fix: Arc<autoimport::Fix>,
    symbol_name: String,
    error_identifier_text: String,
    is_jsx_namespace_fix: bool,
}

// codeactions_importfixes.go:62
fn get_import_code_actions(
    ctx: &Context,
    fix_context: &CodeFixContext,
) -> Result<Vec<CodeAction>, lsproto::Error> {
    let mut ch = fix_context.program.get_type_checker(ctx);

    let info = get_fix_infos(
        &mut ch,
        fix_context,
        fix_context.error_code,
        fix_context.span.pos().expect_source("code-fix spans have source positions"),
    )?;
    if info.is_empty() {
        return Ok(Vec::new());
    }

    let mut actions = Vec::new();
    actions.extend(info.iter().filter_map(|fix_info| {
        let (edits, description, ok) = fix_info.fix.edits(
            ctx,
            fix_context.source_file,
            fix_context.program.options(),
            fix_context.ls.format_options(),
            &fix_context.ls.converters,
            fix_context.ls.user_preferences(),
            fix_info.symbol_name != fix_info.error_identifier_text,
        );

        if ok {
            Some(CodeAction { description, changes: edits, fix_id: IMPORT_FIX_ID.to_string() })
        } else {
            None
        }
    }));
    Ok(actions)
}

// codeactions_importfixes.go:96
fn get_all_import_code_actions(
    ctx: &Context,
    fix_context: &CodeFixContext,
) -> Result<Option<CombinedCodeActions>, lsproto::Error> {
    if tspath::is_dynamic_file_name(fix_context.source_file.file_name()) {
        return Ok(None);
    }

    let all_diagnostics =
        fix_context.program.get_semantic_diagnostics(ctx, Some(fix_context.source_file));

    let mut import_diags: Vec<P<Diagnostic>> = Vec::new();
    import_diags.extend(
        all_diagnostics
            .into_iter()
            .filter(|&diag| is_fixable_diagnostic(diag, import_fix_error_codes())),
    );

    if import_diags.is_empty() {
        return Ok(None);
    }

    let mut ch = fix_context.program.get_type_checker(ctx);

    // Go falls back to the current view when the prepared view is nil; a prepared view is never nil.
    let view = fix_context.ls.get_prepared_auto_import_view(fix_context.source_file, &mut ch)?;

    let mut import_adder = autoimport::new_import_adder(
        ctx,
        fix_context.program,
        &mut ch,
        fix_context.source_file,
        view,
        fix_context.ls.format_options(),
        Arc::clone(&fix_context.ls.converters),
        fix_context.ls.user_preferences().clone(),
    );

    for diag in &import_diags {
        add_import_from_diagnostic(&mut ch, &mut *import_adder, *diag, fix_context)?;
    }

    if !import_adder.has_fixes() {
        return Ok(None);
    }

    Ok(Some(CombinedCodeActions {
        description: diagnostics::ADD_ALL_MISSING_IMPORTS.localize(&[]),
        changes: import_adder.edits(),
    }))
}

// codeactions_importfixes.go:150
// addImportFromDiagnostic finds the best import fix for a diagnostic and adds it to the adder.
fn add_import_from_diagnostic(
    ch: &mut Checker,
    import_adder: &mut dyn ImportAdder,
    diag: P<Diagnostic>,
    fix_context: &CodeFixContext,
) -> Result<(), lsproto::Error> {
    let Some(diag_loc) = diag.loc() else {
        return Ok(());
    };
    let diag_fix_context = CodeFixContext {
        source_file: fix_context.source_file,
        span: diag_loc,
        error_code: diag.code(),
        program: fix_context.program,
        ls: fix_context.ls,
        diagnostic: None,
    };

    let infos = get_fix_infos(
        ch,
        &diag_fix_context,
        diag.code(),
        diag.pos().expect("diagnostic location was checked above"),
    )?;
    if !infos.is_empty() {
        import_adder.add_import_fix(&infos[0].fix);
    }
    Ok(())
}

// codeactions_importfixes.go:169
fn get_fix_infos(
    ch: &mut Checker,
    fix_context: &CodeFixContext,
    error_code: i32,
    pos: u32,
) -> Result<Vec<FixInfo>, lsproto::Error> {
    // Can't compute import fixes for dynamic/untitled files since they don't have real file paths
    if tspath::is_dynamic_file_name(fix_context.source_file.file_name()) {
        return Ok(Vec::new());
    }

    let symbol_token = astnav::get_token_at_position(fix_context.source_file, pos);
    if error_code != diagnostics::X_0_REFERS_TO_A_UMD_GLOBAL_BUT_THE_CURRENT_FILE_IS_A_MODULE_CONSIDER_ADDING_AN_IMPORT_INSTEAD.code()
        && !ast::is_identifier(symbol_token)
    {
        return Ok(Vec::new());
    }

    let view: Option<View>;
    let mut info: Vec<FixInfo>;

    if error_code == diagnostics::X_0_REFERS_TO_A_UMD_GLOBAL_BUT_THE_CURRENT_FILE_IS_A_MODULE_CONSIDER_ADDING_AN_IMPORT_INSTEAD.code() {
        let v = fix_context.ls.get_current_auto_import_view(fix_context.source_file, ch);
        info = get_fixes_info_for_umd_import(symbol_token, &v, ch);
        view = Some(v);
    } else if error_code == diagnostics::X_0_CANNOT_BE_USED_AS_A_VALUE_BECAUSE_IT_WAS_IMPORTED_USING_IMPORT_TYPE.code() {
        let compiler_options = fix_context.program.options();
        let symbol_names = get_symbol_names_to_import(fix_context.source_file, ch, symbol_token, &compiler_options);

        let mut all_type_only_fixes: Vec<FixInfo> = Vec::new();
        all_type_only_fixes.extend(symbol_names.iter().filter_map(|sn| {
            if !sn.is_type_only {
                return None;
            }
            get_type_only_promotion_fix(fix_context.source_file, symbol_token, &sn.name, ch).map(|fix| FixInfo {
                fix: Arc::new(fix),
                symbol_name: sn.name.clone(),
                error_identifier_text: symbol_token.text().to_string(),
                is_jsx_namespace_fix: false,
            })
        }));

        // For JSX opening tags, there can be separate type-only errors for both the tag name
        // identifier and the JSX namespace identifier. When both produce valid fixes, we
        // disambiguate using the diagnostic message, which quotes the symbol name in single
        // quotes (e.g., "'React' cannot be used as a value..."). If filtering yields nothing
        // (e.g., due to localization), fall back to returning all candidates.
        let mut diagnostic_message = String::new();
        if let Some(d) = fix_context.diagnostic {
            diagnostic_message = d.message.as_string();
        }
        if all_type_only_fixes.len() > 1 && !diagnostic_message.is_empty() {
            let (matching, rest): (Vec<FixInfo>, Vec<FixInfo>) =
                all_type_only_fixes.into_iter().partition(|fi| diagnostic_message.contains(&format!("'{}'", fi.symbol_name)));
            info = matching;
            all_type_only_fixes = rest;
            if info.is_empty() {
                info = all_type_only_fixes;
            }
            return Ok(info);
        }
        return Ok(all_type_only_fixes);
    } else {
        let v = fix_context.ls.get_prepared_auto_import_view(fix_context.source_file, ch)?;
        info = get_fixes_info_for_non_umd_import(fix_context, symbol_token, &v, ch);
        view = Some(v);
    }

    // Sort fixes by preference
    let view = view.unwrap_or_else(|| {
        fix_context.ls.get_current_auto_import_view(fix_context.source_file, ch)
    });
    Ok(sort_fix_info(info, &view))
}

// codeactions_importfixes.go:240
fn get_fixes_info_for_umd_import(token: P<Node>, view: &View, ch: &mut Checker) -> Vec<FixInfo> {
    let Some(umd_symbol) = get_umd_symbol(token, ch) else {
        return Vec::new();
    };

    // Go passes the (possibly nil) export on; GetFixes dereferences it.
    let export = autoimport::symbol_to_export(umd_symbol, ch).expect("nil export");
    let is_valid_type_only_use_site = ast::is_valid_type_only_alias_use_site(token);

    let mut result = Vec::new();
    result.extend(
        view.get_fixes(&export, false, is_valid_type_only_use_site, None).into_iter().map(|fix| {
            FixInfo {
                fix,
                symbol_name: umd_symbol.name().to_string(),
                error_identifier_text: if ast::is_identifier(token) {
                    token.text().to_string()
                } else {
                    String::new()
                },
                is_jsx_namespace_fix: false,
            }
        }),
    );
    result
}

// codeactions_importfixes.go:264
fn get_umd_symbol(token: P<Node>, ch: &mut Checker) -> Option<P<Symbol>> {
    // try the identifier to see if it is the umd symbol
    let mut umd_symbol: Option<P<Symbol>> = None;
    if ast::is_identifier(token) {
        umd_symbol = Some(ch.get_resolved_symbol_exported(token));
    }
    if is_umd_export_symbol(umd_symbol) {
        return umd_symbol;
    }

    // The error wasn't for the symbolAtLocation, it was for the JSX tag itself, which needs access to e.g. `React`.
    let parent = token.parent().unwrap();
    if (ast::is_jsx_opening_like_element(parent) && parent.tag_name() == token)
        || ast::is_jsx_opening_fragment(parent)
    {
        let location = if ast::is_jsx_opening_like_element(parent) { token } else { parent };
        let jsx_namespace = ch.get_jsx_namespace_exported(parent);
        let parent_symbol = ch.resolve_name_exported(
            &jsx_namespace,
            location,
            SymbolFlags::Value,
            false, /* excludeGlobals */
        );
        if is_umd_export_symbol(parent_symbol) {
            return parent_symbol;
        }
    }
    None
}

// codeactions_importfixes.go:291
fn is_umd_export_symbol(symbol: Option<P<Symbol>>) -> bool {
    symbol.is_some_and(|s| {
        !s.declarations().is_empty() && ast::is_namespace_export_declaration(s.declarations()[0])
    })
}

// codeactions_importfixes.go:297
fn get_fixes_info_for_non_umd_import(
    fix_context: &CodeFixContext,
    symbol_token: P<Node>,
    view: &View,
    ch: &mut Checker,
) -> Vec<FixInfo> {
    let compiler_options = fix_context.program.options();

    let is_valid_type_only_use_site = ast::is_valid_type_only_alias_use_site(symbol_token);
    let symbol_names =
        get_symbol_names_to_import(fix_context.source_file, ch, symbol_token, &compiler_options);
    let mut all_info: Vec<FixInfo> = Vec::new();

    // Compute usage position for JSDoc import type fixes
    let (usage_position, fidelity) = fix_context.ls.converters.to_lsp_position(
        &fix_context.source_file,
        scanner::get_token_pos_of_node(symbol_token, fix_context.source_file, false).into(),
    );
    if !fidelity.is_exact() {
        return Vec::new();
    }

    for sn in &symbol_names {
        // Type-only imports are handled by the promotion code path, not the auto-import path.
        if sn.is_type_only {
            continue;
        }

        let symbol_name = &sn.name;
        // "default" is a keyword and not a legal identifier for the import
        if symbol_name == "default" {
            continue;
        }

        let is_jsx_tag_name =
            symbol_name == symbol_token.text() && ast::is_jsx_tag_name(symbol_token);
        let mut query_kind = QueryKind::ExactMatch;
        if is_jsx_tag_name {
            query_kind = QueryKind::CaseInsensitiveMatch;
        }

        let exports = view.search(symbol_name, query_kind);
        for export in &exports {
            if is_jsx_tag_name && !(export.name() == symbol_name || export.is_renameable()) {
                continue;
            }

            let fixes = view.get_fixes(
                export,
                is_jsx_tag_name,
                is_valid_type_only_use_site,
                Some(usage_position),
            );
            all_info.extend(fixes.into_iter().map(|fix| FixInfo {
                fix,
                symbol_name: symbol_name.clone(),
                error_identifier_text: symbol_token.text().to_string(),
                is_jsx_namespace_fix: symbol_name != symbol_token.text(),
            }));
        }
    }

    all_info
}

// codeactions_importfixes.go:351
fn get_type_only_promotion_fix(
    source_file: P<SourceFile>,
    symbol_token: P<Node>,
    symbol_name: &str,
    ch: &mut Checker,
) -> Option<autoimport::Fix> {
    // Get the symbol at the token location
    let symbol = ch.resolve_name_exported(
        symbol_name,
        symbol_token,
        SymbolFlags::Value,
        true, /* excludeGlobals */
    )?;

    // Get the type-only alias declaration
    let type_only_alias_declaration = ch.get_type_only_alias_declaration(symbol)?;
    if ast::get_source_file_of_node(type_only_alias_declaration) != Some(source_file) {
        return None;
    }

    Some(autoimport::Fix {
        auto_import_fix: lsproto::AutoImportFix {
            kind: lsproto::AutoImportFixKind::PROMOTE_TYPE_ONLY,
            ..Default::default()
        },
        type_only_alias_declaration: Some(type_only_alias_declaration),
        ..Default::default()
    })
}

// codeactions_importfixes.go:372
struct SymbolNameInfo {
    name: String,
    is_type_only: bool, // whether the symbol currently resolves to a type-only import
}

// codeactions_importfixes.go:377
fn get_symbol_names_to_import(
    source_file: P<SourceFile>,
    ch: &mut Checker,
    symbol_token: P<Node>,
    compiler_options: &CompilerOptions,
) -> Vec<SymbolNameInfo> {
    let parent = symbol_token.parent().unwrap();
    if (ast::is_jsx_opening_like_element(parent) || ast::is_jsx_closing_element(parent))
        && parent.tag_name() == symbol_token
        && jsx_mode_needs_explicit_import(compiler_options.jsx)
    {
        let jsx_namespace = ch.get_jsx_namespace_exported(source_file.as_node());
        if needs_jsx_namespace_fix(&jsx_namespace, symbol_token, ch) {
            let mut result = Vec::new();
            if !scanner::is_intrinsic_jsx_name(symbol_token.text()) {
                let comp_symbol = ch.resolve_name_exported(
                    symbol_token.text(),
                    symbol_token,
                    SymbolFlags::Value,
                    false, /* excludeGlobals */
                );
                match comp_symbol {
                    None => result.push(SymbolNameInfo {
                        name: symbol_token.text().to_string(),
                        is_type_only: false,
                    }),
                    Some(comp_symbol) => {
                        if ch.get_type_only_alias_declaration(comp_symbol).is_some() {
                            result.push(SymbolNameInfo {
                                name: symbol_token.text().to_string(),
                                is_type_only: true,
                            });
                        }
                    }
                }
            }
            let mut ns_is_type_only = false;
            if let Some(ns_symbol) = ch.resolve_name_exported(
                &jsx_namespace,
                symbol_token,
                SymbolFlags::Value,
                true, /* excludeGlobals */
            ) {
                ns_is_type_only = ch.get_type_only_alias_declaration(ns_symbol).is_some();
            }
            result.push(SymbolNameInfo { name: jsx_namespace, is_type_only: ns_is_type_only });
            return result;
        }
    }
    let mut token_is_type_only = false;
    if let Some(sym) = ch.resolve_name_exported(
        symbol_token.text(),
        symbol_token,
        SymbolFlags::Value,
        true, /* excludeGlobals */
    ) {
        token_is_type_only = ch.get_type_only_alias_declaration(sym).is_some();
    }
    vec![SymbolNameInfo { name: symbol_token.text().to_string(), is_type_only: token_is_type_only }]
}

// codeactions_importfixes.go:411
fn needs_jsx_namespace_fix(jsx_namespace: &str, symbol_token: P<Node>, ch: &mut Checker) -> bool {
    if scanner::is_intrinsic_jsx_name(symbol_token.text()) {
        return true;
    }
    let Some(namespace_symbol) = ch.resolve_name_exported(
        jsx_namespace,
        symbol_token,
        SymbolFlags::Value,
        true, /* excludeGlobals */
    ) else {
        return true;
    };
    if namespace_symbol
        .declarations()
        .iter()
        .any(|&d| ast::is_type_only_import_or_export_declaration(d))
    {
        return !namespace_symbol.flags().intersects(SymbolFlags::Value);
    }
    false
}

// codeactions_importfixes.go:425
fn jsx_mode_needs_explicit_import(jsx: JsxEmit) -> bool {
    jsx == JsxEmit::REACT || jsx == JsxEmit::REACT_NATIVE
}

// codeactions_importfixes.go:429
fn sort_fix_info(fixes: Vec<FixInfo>, view: &View) -> Vec<FixInfo> {
    if fixes.is_empty() {
        return fixes;
    }

    // Create a copy to avoid modifying the original
    let mut sorted = fixes;

    // Sort by:
    // 1. JSX namespace fixes last
    // 2. Fix comparison using view.CompareFixes
    tsrs_core::goslices::sort_func(&mut sorted, |a, b| {
        // JSX namespace fixes should come last
        let cmp = tsrs_core::compare_booleans(a.is_jsx_namespace_fix, b.is_jsx_namespace_fix);
        if !cmp.is_eq() {
            return cmp;
        }
        view.compare_fixes_for_sorting(&a.fix, &b.fix)
    });

    sorted
}
