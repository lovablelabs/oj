use std::borrow::Cow;
use std::sync::OnceLock;

use rustc_hash::FxHashMap;
use tsrs_core::Tristate;
use tsrs_core::json::{JsonMap, Value};
use tsrs_modulespecifiers::{
    ImportModuleSpecifierEndingPreference, ImportModuleSpecifierPreference,
};
use tsrs_vfs::vfsmatch;

use super::*;

// userpreferences.go:16
pub fn new_default_user_preferences() -> UserPreferences {
    UserPreferences {
        format_code_settings: get_default_format_code_settings(),

        include_completions_for_module_exports: Tristate::True,
        include_completions_for_import_statements: Tristate::True,
        enable_auto_closing_tags: Tristate::True,
        enable_jsdoc_completions: Tristate::True,
        generate_return_in_doc_template: Tristate::True,

        allow_rename_of_import_path: Tristate::True,
        provide_refactor_not_applicable_reason: Tristate::True,
        enable_formatting: Tristate::True,
        enable_validation: Tristate::True,
        display_parts_for_jsdoc: Tristate::True,
        disable_line_text_in_references: Tristate::True,
        report_style_checks_as_warnings: Tristate::True,

        exclude_library_symbols_in_nav_to: Tristate::True,
        workspace_symbols_scope: WorkspaceSymbolsScope::ALL_OPEN_PROJECTS,
        ..Default::default()
    }
}

// userpreferences.go:47
// UserPreferences represents TypeScript language service preferences.
//
// Go populates fields using two struct tags, `raw:"name[,invert]"` (TypeScript/raw name for unstable section lookup)
// and `config:"path.to.setting[,invert]"` (VS Code nested config path), plus `fallbackConfig`. The tags live in
// USER_PREFERENCES_FIELD_TAGS below. The `,invert` modifier inverts boolean values (e.g., VS Code's "suppress" -> our
// "include").
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct UserPreferences {
    pub format_code_settings: FormatCodeSettings,

    pub quote_preference: QuotePreference,
    pub lazy_configured_projects_from_external_project: Tristate, // !!!

    // A positive integer indicating the maximum length of a hover text before it is truncated.
    //
    // Default: `500`
    pub maximum_hover_length: i32, // !!!

    // ------- Completions -------

    // If enabled, TypeScript will search through all external modules' exports and add them to the completions list.
    // This affects lone identifier completions but not completions on the right hand side of `obj.`.
    pub include_completions_for_module_exports: Tristate,
    // Enables auto-import-style completions on partially-typed import statements. E.g., allows
    // `import write|` to be completed to `import { writeFile } from "fs"`.
    pub include_completions_for_import_statements: Tristate,
    // Unless this option is `false`,  member completion lists triggered with `.` will include entries
    // on potentially-null and potentially-undefined values, with insertion text to replace
    // preceding `.` tokens with `?.`.
    pub include_automatic_optional_chain_completions: Tristate,
    // If enabled, completions for class members (e.g. methods and properties) will include
    // a whole declaration for the member.
    // E.g., `class A { f| }` could be completed to `class A { foo(): number {} }`, instead of
    // `class A { foo }`.
    pub include_completions_with_class_member_snippets: Tristate,
    // If enabled, object literal methods will have a method declaration completion entry in addition
    // to the regular completion entry containing just the method name.
    // E.g., `const objectLiteral: T = { f| }` could be completed to `const objectLiteral: T = { foo(): void {} }`,
    // in addition to `const objectLiteral: T = { foo }`.
    pub include_completions_with_object_literal_method_snippets: Tristate,
    pub jsx_attribute_completion_style: JsxAttributeCompletionStyle,
    pub enable_auto_closing_tags: Tristate,
    pub enable_jsdoc_completions: Tristate,
    pub generate_return_in_doc_template: Tristate,

    // ------- AutoImports --------
    pub import_module_specifier_preference: ImportModuleSpecifierPreference, // !!!
    // Determines whether we import `foo/index.ts` as "foo", "foo/index", or "foo/index.js"
    pub import_module_specifier_ending: ImportModuleSpecifierEndingPreference, // !!!
    pub auto_import_specifier_exclude_regexes: Option<Vec<String>>,            // !!!
    pub auto_import_file_exclude_patterns: Option<Vec<String>>,
    pub auto_import_entrypoint_directory_search: Tristate,
    pub prefer_type_only_auto_imports: Tristate,

    // ------- OrganizeImports -------

    // Indicates which deterministic preset should be used to sort imports.
    // "auto" detects the existing ordinal case sensitivity where possible.
    pub organize_imports_sort: OrganizeImportsSort, // !!!
    // Indicates whether imports should be organized in a case-insensitive manner.
    //
    // Default: TSUnknown ("auto" in strada), will perform detection
    pub organize_imports_ignore_case: Tristate, // !!!
    // Indicates whether imports should be organized via an "ordinal" (binary) comparison using the numeric value of their
    // code points, or via "unicode" natural sorting. This implementation is locale-agnostic and approximates the practical
    // import-sorting behavior rather than the full Unicode Collation Algorithm.
    //
    // Default: Ordinal
    pub organize_imports_collation: OrganizeImportsCollation, // !!!
    // Indicates the locale to use for "unicode" collation in legacy clients. This is accepted for compatibility, but
    // currently ignored because organize-import sorting is deterministic and locale-agnostic.
    //
    // This preference is ignored if organizeImportsCollation is not `unicode`.
    //
    // Default: `"en"`
    pub organize_imports_locale: String, // !!!
    // Indicates whether numeric collation should be used for digit sequences in strings. When `true`, will collate
    // strings such that `a1z < a2z < a100z`. When `false`, will collate strings such that `a1z < a100z < a2z`.
    //
    // This preference is ignored if organizeImportsCollation is not `unicode`.
    //
    // Default: `false`
    pub organize_imports_numeric_collation: Tristate, // !!!
    // Indicates whether accents and other diacritic marks are considered unequal for the purpose of sorting.
    //
    // This preference is ignored if organizeImportsCollation is not `unicode`.
    //
    // Default: `true`
    pub organize_imports_accent_collation: Tristate, // !!!
    // Indicates whether upper case or lower case should sort first.
    //
    // This permission is ignored if:
    //	- organizeImportsCollation is not `unicode`
    //	- organizeImportsIgnoreCase is `true`
    //	- organizeImportsIgnoreCase is `auto` and the auto-detected case sensitivity is case-insensitive.
    //
    // Default: `false`
    pub organize_imports_case_first: OrganizeImportsCaseFirst, // !!!
    // Indicates where named type-only imports should sort. "inline" sorts named imports without regard to if the import is type-only.
    //
    // Default: `auto`, which defaults to `last`
    pub organize_imports_type_order: OrganizeImportsTypeOrder, // !!!

    // ------- MoveToFile -------
    pub allow_text_changes_in_new_files: Tristate, // !!!

    // ------- Rename -------
    pub use_aliases_for_rename: Tristate,
    pub allow_rename_of_import_path: Tristate,

    // ------- CodeFixes/Refactors -------
    pub provide_refactor_not_applicable_reason: Tristate, // !!!

    // ------- InlayHints -------
    pub inlay_hints: InlayHintsPreferences,

    // ------- CodeLens -------
    pub code_lens: CodeLensUserPreferences,

    // ------- Definition -------
    pub prefer_go_to_source_definition: bool,

    // ------- Symbols -------
    pub exclude_library_symbols_in_nav_to: Tristate,
    pub workspace_symbols_scope: WorkspaceSymbolsScope,

    // ------- Misc -------
    pub enable_formatting: Tristate,
    pub enable_validation: Tristate,
    pub disable_suggestions: Tristate,             // !!!
    pub disable_line_text_in_references: Tristate, // !!!
    pub display_parts_for_jsdoc: Tristate,         // !!!
    pub report_style_checks_as_warnings: Tristate,
    pub locale: String,

    // ------- ATA -------

    // DisableAutomaticTypeAcquisition is the deprecated setting from typescript.disableAutomaticTypeAcquisition.
    pub disable_automatic_type_acquisition: Tristate,
    // AutomaticTypeAcquisitionEnabled is the unified setting from tsserver.automaticTypeAcquisition.enabled under the js/ts section.
    // When set, it takes precedence over DisableAutomaticTypeAcquisition.
    pub automatic_type_acquisition_enabled: Tristate,
    // TODO: add tsserver.web.typeAcquisition.enabled under the js/ts section for the web variant when web support is implemented.

    // ------- Project Configuration -------

    // CustomConfigFileName specifies a custom config file name to use before defaulting to tsconfig.json/jsconfig.json.
    pub custom_config_file_name: String,
}

// userpreferences.go:47 (the struct tags of UserPreferences and the structs it embeds, flattened in declaration order)
// Source12's Go parser is generated from the preference schema. This Rust table retains the same raw/config
// field names and accessors; decoding it matches the generated parser without Go's code generator.
pub struct FieldTag {
    pub name: &'static str,
    raw: &'static str,
    config: &'static str,
    fallback_config: &'static str,
    pub get: for<'a> fn(&'a UserPreferences) -> FieldRef<'a>,
    pub get_mut: for<'a> fn(&'a mut UserPreferences) -> FieldMut<'a>,
}

// Field types determine the parsers and serializers for each preference.
#[derive(Clone, Copy)]
pub enum FieldRef<'a> {
    Tristate(&'a Tristate),
    IndentStyle(&'a IndentStyle),
    SemicolonPreference(&'a SemicolonPreference),
    QuotePreference(&'a QuotePreference),
    JsxAttributeCompletionStyle(&'a JsxAttributeCompletionStyle),
    IncludeInlayParameterNameHints(&'a IncludeInlayParameterNameHints),
    OrganizeImportsSort(&'a OrganizeImportsSort),
    OrganizeImportsCollation(&'a OrganizeImportsCollation),
    OrganizeImportsCaseFirst(&'a OrganizeImportsCaseFirst),
    OrganizeImportsTypeOrder(&'a OrganizeImportsTypeOrder),
    ImportModuleSpecifierPreference(&'a ImportModuleSpecifierPreference),
    ImportModuleSpecifierEndingPreference(&'a ImportModuleSpecifierEndingPreference),
    WorkspaceSymbolsScope(&'a WorkspaceSymbolsScope),
    Bool(&'a bool),
    Int(&'a i32),
    String(&'a String),
    Slice(&'a Option<Vec<String>>),
}

pub enum FieldMut<'a> {
    Tristate(&'a mut Tristate),
    IndentStyle(&'a mut IndentStyle),
    SemicolonPreference(&'a mut SemicolonPreference),
    QuotePreference(&'a mut QuotePreference),
    JsxAttributeCompletionStyle(&'a mut JsxAttributeCompletionStyle),
    IncludeInlayParameterNameHints(&'a mut IncludeInlayParameterNameHints),
    OrganizeImportsSort(&'a mut OrganizeImportsSort),
    OrganizeImportsCollation(&'a mut OrganizeImportsCollation),
    OrganizeImportsCaseFirst(&'a mut OrganizeImportsCaseFirst),
    OrganizeImportsTypeOrder(&'a mut OrganizeImportsTypeOrder),
    ImportModuleSpecifierPreference(&'a mut ImportModuleSpecifierPreference),
    ImportModuleSpecifierEndingPreference(&'a mut ImportModuleSpecifierEndingPreference),
    WorkspaceSymbolsScope(&'a mut WorkspaceSymbolsScope),
    Bool(&'a mut bool),
    Int(&'a mut i32),
    String(&'a mut String),
    Slice(&'a mut Option<Vec<String>>),
}

macro_rules! field_tag {
    ($kind:ident, $($path:ident).+, $raw:literal, $config:literal) => {
        field_tag!($kind, $($path).+, $raw, $config, "")
    };
    ($kind:ident, $($path:ident).+, $raw:literal, $config:literal, $fallback:literal) => {
        FieldTag {
            name: stringify!($($path).+),
            raw: $raw,
            config: $config,
            fallback_config: $fallback,
            get: |p| FieldRef::$kind(&p.$($path).+),
            get_mut: |p| FieldMut::$kind(&mut p.$($path).+),
        }
    };
}

pub static USER_PREFERENCES_FIELD_TAGS: &[FieldTag] = &[
    // FormatCodeSettings.EditorSettings
    field_tag!(
        Int,
        format_code_settings.editor_settings.base_indent_size,
        "baseIndentSize",
        "format.baseIndentSize"
    ),
    field_tag!(
        Int,
        format_code_settings.editor_settings.indent_size,
        "indentSize",
        "format.indentSize"
    ),
    field_tag!(Int, format_code_settings.editor_settings.tab_size, "tabSize", "format.tabSize"),
    field_tag!(
        String,
        format_code_settings.editor_settings.new_line_character,
        "newLineCharacter",
        "format.newLineCharacter"
    ),
    field_tag!(
        Tristate,
        format_code_settings.editor_settings.convert_tabs_to_spaces,
        "convertTabsToSpaces",
        "format.convertTabsToSpaces"
    ),
    field_tag!(
        IndentStyle,
        format_code_settings.editor_settings.indent_style,
        "indentStyle",
        "format.indentStyle"
    ),
    field_tag!(
        Tristate,
        format_code_settings.editor_settings.trim_trailing_whitespace,
        "trimTrailingWhitespace",
        "format.trimTrailingWhitespace"
    ),
    // FormatCodeSettings
    field_tag!(
        Tristate,
        format_code_settings.insert_space_after_comma_delimiter,
        "insertSpaceAfterCommaDelimiter",
        "format.insertSpaceAfterCommaDelimiter"
    ),
    field_tag!(
        Tristate,
        format_code_settings.insert_space_after_semicolon_in_for_statements,
        "insertSpaceAfterSemicolonInForStatements",
        "format.insertSpaceAfterSemicolonInForStatements"
    ),
    field_tag!(
        Tristate,
        format_code_settings.insert_space_before_and_after_binary_operators,
        "insertSpaceBeforeAndAfterBinaryOperators",
        "format.insertSpaceBeforeAndAfterBinaryOperators"
    ),
    field_tag!(
        Tristate,
        format_code_settings.insert_space_after_constructor,
        "insertSpaceAfterConstructor",
        "format.insertSpaceAfterConstructor"
    ),
    field_tag!(
        Tristate,
        format_code_settings.insert_space_after_keywords_in_control_flow_statements,
        "insertSpaceAfterKeywordsInControlFlowStatements",
        "format.insertSpaceAfterKeywordsInControlFlowStatements"
    ),
    field_tag!(
        Tristate,
        format_code_settings.insert_space_after_function_keyword_for_anonymous_functions,
        "insertSpaceAfterFunctionKeywordForAnonymousFunctions",
        "format.insertSpaceAfterFunctionKeywordForAnonymousFunctions"
    ),
    field_tag!(
        Tristate,
        format_code_settings.insert_space_after_opening_and_before_closing_nonempty_parenthesis,
        "insertSpaceAfterOpeningAndBeforeClosingNonemptyParenthesis",
        "format.insertSpaceAfterOpeningAndBeforeClosingNonemptyParenthesis"
    ),
    field_tag!(
        Tristate,
        format_code_settings.insert_space_after_opening_and_before_closing_nonempty_brackets,
        "insertSpaceAfterOpeningAndBeforeClosingNonemptyBrackets",
        "format.insertSpaceAfterOpeningAndBeforeClosingNonemptyBrackets"
    ),
    field_tag!(
        Tristate,
        format_code_settings.insert_space_after_opening_and_before_closing_nonempty_braces,
        "insertSpaceAfterOpeningAndBeforeClosingNonemptyBraces",
        "format.insertSpaceAfterOpeningAndBeforeClosingNonemptyBraces"
    ),
    field_tag!(
        Tristate,
        format_code_settings.insert_space_after_opening_and_before_closing_empty_braces,
        "insertSpaceAfterOpeningAndBeforeClosingEmptyBraces",
        "format.insertSpaceAfterOpeningAndBeforeClosingEmptyBraces"
    ),
    field_tag!(
        Tristate,
        format_code_settings.insert_space_after_opening_and_before_closing_template_string_braces,
        "insertSpaceAfterOpeningAndBeforeClosingTemplateStringBraces",
        "format.insertSpaceAfterOpeningAndBeforeClosingTemplateStringBraces"
    ),
    field_tag!(
        Tristate,
        format_code_settings.insert_space_after_opening_and_before_closing_jsx_expression_braces,
        "insertSpaceAfterOpeningAndBeforeClosingJsxExpressionBraces",
        "format.insertSpaceAfterOpeningAndBeforeClosingJsxExpressionBraces"
    ),
    field_tag!(
        Tristate,
        format_code_settings.insert_space_after_type_assertion,
        "insertSpaceAfterTypeAssertion",
        "format.insertSpaceAfterTypeAssertion"
    ),
    field_tag!(
        Tristate,
        format_code_settings.insert_space_before_function_parenthesis,
        "insertSpaceBeforeFunctionParenthesis",
        "format.insertSpaceBeforeFunctionParenthesis"
    ),
    field_tag!(
        Tristate,
        format_code_settings.place_open_brace_on_new_line_for_functions,
        "placeOpenBraceOnNewLineForFunctions",
        "format.placeOpenBraceOnNewLineForFunctions"
    ),
    field_tag!(
        Tristate,
        format_code_settings.place_open_brace_on_new_line_for_control_blocks,
        "placeOpenBraceOnNewLineForControlBlocks",
        "format.placeOpenBraceOnNewLineForControlBlocks"
    ),
    field_tag!(
        Tristate,
        format_code_settings.insert_space_before_type_annotation,
        "insertSpaceBeforeTypeAnnotation",
        "format.insertSpaceBeforeTypeAnnotation"
    ),
    field_tag!(
        Tristate,
        format_code_settings.indent_multi_line_object_literal_beginning_on_blank_line,
        "indentMultiLineObjectLiteralBeginningOnBlankLine",
        "format.indentMultiLineObjectLiteralBeginningOnBlankLine"
    ),
    field_tag!(
        SemicolonPreference,
        format_code_settings.semicolons,
        "semicolons",
        "format.semicolons"
    ),
    field_tag!(
        Tristate,
        format_code_settings.indent_switch_case,
        "indentSwitchCase",
        "format.indentSwitchCase"
    ),
    // UserPreferences
    field_tag!(QuotePreference, quote_preference, "quotePreference", "preferences.quoteStyle"),
    field_tag!(
        Tristate,
        lazy_configured_projects_from_external_project,
        "lazyConfiguredProjectsFromExternalProject",
        ""
    ),
    field_tag!(Int, maximum_hover_length, "maximumHoverLength", ""),
    field_tag!(
        Tristate,
        include_completions_for_module_exports,
        "includeCompletionsForModuleExports",
        "suggest.autoImports"
    ),
    field_tag!(
        Tristate,
        include_completions_for_import_statements,
        "includeCompletionsForImportStatements",
        "suggest.includeCompletionsForImportStatements"
    ),
    field_tag!(
        Tristate,
        include_automatic_optional_chain_completions,
        "includeAutomaticOptionalChainCompletions",
        "suggest.includeAutomaticOptionalChainCompletions"
    ),
    field_tag!(
        Tristate,
        include_completions_with_class_member_snippets,
        "includeCompletionsWithClassMemberSnippets",
        "suggest.classMemberSnippets.enabled"
    ),
    field_tag!(
        Tristate,
        include_completions_with_object_literal_method_snippets,
        "includeCompletionsWithObjectLiteralMethodSnippets",
        "suggest.objectLiteralMethodSnippets.enabled"
    ),
    field_tag!(
        JsxAttributeCompletionStyle,
        jsx_attribute_completion_style,
        "jsxAttributeCompletionStyle",
        "preferences.jsxAttributeCompletionStyle"
    ),
    field_tag!(
        Tristate,
        enable_auto_closing_tags,
        "autoClosingTags",
        "autoClosingTags.enabled",
        "autoClosingTags"
    ),
    field_tag!(
        Tristate,
        enable_jsdoc_completions,
        "completeJSDocs",
        "suggest.jsdoc.enabled",
        "suggest.completeJSDocs"
    ),
    field_tag!(
        Tristate,
        generate_return_in_doc_template,
        "generateReturnInDocTemplate",
        "suggest.jsdoc.generateReturns"
    ),
    field_tag!(
        ImportModuleSpecifierPreference,
        import_module_specifier_preference,
        "importModuleSpecifierPreference",
        "preferences.importModuleSpecifier"
    ),
    field_tag!(
        ImportModuleSpecifierEndingPreference,
        import_module_specifier_ending,
        "importModuleSpecifierEnding",
        "preferences.importModuleSpecifierEnding"
    ),
    field_tag!(
        Slice,
        auto_import_specifier_exclude_regexes,
        "autoImportSpecifierExcludeRegexes",
        "preferences.autoImportSpecifierExcludeRegexes"
    ),
    field_tag!(
        Slice,
        auto_import_file_exclude_patterns,
        "autoImportFileExcludePatterns",
        "preferences.autoImportFileExcludePatterns"
    ),
    field_tag!(
        Tristate,
        auto_import_entrypoint_directory_search,
        "autoImportEntrypointDirectorySearch",
        "preferences.autoImportEntrypointDirectorySearch"
    ),
    field_tag!(
        Tristate,
        prefer_type_only_auto_imports,
        "preferTypeOnlyAutoImports",
        "preferences.preferTypeOnlyAutoImports"
    ),
    field_tag!(
        OrganizeImportsSort,
        organize_imports_sort,
        "organizeImportsSort",
        "preferences.organizeImports.sort"
    ),
    field_tag!(
        Tristate,
        organize_imports_ignore_case,
        "organizeImportsIgnoreCase",
        "preferences.organizeImports.caseSensitivity"
    ),
    field_tag!(
        OrganizeImportsCollation,
        organize_imports_collation,
        "organizeImportsCollation",
        "preferences.organizeImports.unicodeCollation"
    ),
    field_tag!(
        String,
        organize_imports_locale,
        "organizeImportsLocale",
        "preferences.organizeImports.locale"
    ),
    field_tag!(
        Tristate,
        organize_imports_numeric_collation,
        "organizeImportsNumericCollation",
        "preferences.organizeImports.numericCollation"
    ),
    field_tag!(
        Tristate,
        organize_imports_accent_collation,
        "organizeImportsAccentCollation",
        "preferences.organizeImports.accentCollation"
    ),
    field_tag!(
        OrganizeImportsCaseFirst,
        organize_imports_case_first,
        "organizeImportsCaseFirst",
        "preferences.organizeImports.caseFirst"
    ),
    field_tag!(
        OrganizeImportsTypeOrder,
        organize_imports_type_order,
        "organizeImportsTypeOrder",
        "preferences.organizeImports.typeOrder"
    ),
    field_tag!(Tristate, allow_text_changes_in_new_files, "allowTextChangesInNewFiles", ""),
    field_tag!(
        Tristate,
        use_aliases_for_rename,
        "providePrefixAndSuffixTextForRename",
        "preferences.useAliasesForRenames"
    ),
    field_tag!(Tristate, allow_rename_of_import_path, "allowRenameOfImportPath", ""),
    field_tag!(
        Tristate,
        provide_refactor_not_applicable_reason,
        "provideRefactorNotApplicableReason",
        ""
    ),
    // UserPreferences.InlayHints
    field_tag!(
        IncludeInlayParameterNameHints,
        inlay_hints.include_inlay_parameter_name_hints,
        "includeInlayParameterNameHints",
        "inlayHints.parameterNames.enabled"
    ),
    field_tag!(
        Tristate,
        inlay_hints.include_inlay_parameter_name_hints_when_argument_matches_name,
        "includeInlayParameterNameHintsWhenArgumentMatchesName",
        "inlayHints.parameterNames.suppressWhenArgumentMatchesName,invert"
    ),
    field_tag!(
        Tristate,
        inlay_hints.include_inlay_function_parameter_type_hints,
        "includeInlayFunctionParameterTypeHints",
        "inlayHints.parameterTypes.enabled"
    ),
    field_tag!(
        Tristate,
        inlay_hints.include_inlay_variable_type_hints,
        "includeInlayVariableTypeHints",
        "inlayHints.variableTypes.enabled"
    ),
    field_tag!(
        Tristate,
        inlay_hints.include_inlay_variable_type_hints_when_type_matches_name,
        "includeInlayVariableTypeHintsWhenTypeMatchesName",
        "inlayHints.variableTypes.suppressWhenTypeMatchesName,invert"
    ),
    field_tag!(
        Tristate,
        inlay_hints.include_inlay_property_declaration_type_hints,
        "includeInlayPropertyDeclarationTypeHints",
        "inlayHints.propertyDeclarationTypes.enabled"
    ),
    field_tag!(
        Tristate,
        inlay_hints.include_inlay_function_like_return_type_hints,
        "includeInlayFunctionLikeReturnTypeHints",
        "inlayHints.functionLikeReturnTypes.enabled"
    ),
    field_tag!(
        Tristate,
        inlay_hints.include_inlay_enum_member_value_hints,
        "includeInlayEnumMemberValueHints",
        "inlayHints.enumMemberValues.enabled"
    ),
    // UserPreferences.CodeLens
    field_tag!(
        Tristate,
        code_lens.references_code_lens_enabled,
        "referencesCodeLensEnabled",
        "referencesCodeLens.enabled"
    ),
    field_tag!(
        Tristate,
        code_lens.implementations_code_lens_enabled,
        "implementationsCodeLensEnabled",
        "implementationsCodeLens.enabled"
    ),
    field_tag!(
        Tristate,
        code_lens.references_code_lens_show_on_all_functions,
        "referencesCodeLensShowOnAllFunctions",
        "referencesCodeLens.showOnAllFunctions"
    ),
    field_tag!(
        Tristate,
        code_lens.implementations_code_lens_show_on_interface_methods,
        "implementationsCodeLensShowOnInterfaceMethods",
        "implementationsCodeLens.showOnInterfaceMethods"
    ),
    field_tag!(
        Tristate,
        code_lens.implementations_code_lens_show_on_all_class_methods,
        "implementationsCodeLensShowOnAllClassMethods",
        "implementationsCodeLens.showOnAllClassMethods"
    ),
    // UserPreferences (continued)
    field_tag!(Bool, prefer_go_to_source_definition, "preferGoToSourceDefinition", ""),
    field_tag!(
        Tristate,
        exclude_library_symbols_in_nav_to,
        "excludeLibrarySymbolsInNavTo",
        "workspaceSymbols.excludeLibrarySymbols"
    ),
    field_tag!(
        WorkspaceSymbolsScope,
        workspace_symbols_scope,
        "workspaceSymbolsScope",
        "workspaceSymbols.scope"
    ),
    field_tag!(Tristate, enable_formatting, "formatEnabled", "format.enabled", "format.enable"),
    field_tag!(
        Tristate,
        enable_validation,
        "validateEnabled",
        "validate.enabled",
        "validate.enable"
    ),
    field_tag!(Tristate, disable_suggestions, "disableSuggestions", ""),
    field_tag!(Tristate, disable_line_text_in_references, "disableLineTextInReferences", ""),
    field_tag!(Tristate, display_parts_for_jsdoc, "displayPartsForJSDoc", ""),
    field_tag!(
        Tristate,
        report_style_checks_as_warnings,
        "reportStyleChecksAsWarnings",
        "reportStyleChecksAsWarnings"
    ),
    field_tag!(String, locale, "locale", "locale"),
    field_tag!(
        Tristate,
        disable_automatic_type_acquisition,
        "disableAutomaticTypeAcquisition",
        "disableAutomaticTypeAcquisition"
    ),
    field_tag!(
        Tristate,
        automatic_type_acquisition_enabled,
        "automaticTypeAcquisitionEnabled",
        "tsserver.automaticTypeAcquisition.enabled"
    ),
    field_tag!(String, custom_config_file_name, "customConfigFileName", "customConfigFileName"),
];

impl UserPreferences {
    // userpreferences.go:202
    // IsATADisabled returns whether Automatic Type Acquisition is disabled based on user preferences.
    // It checks the unified setting (tsserver.automaticTypeAcquisition.enabled) first,
    // then falls back to the deprecated setting (disableAutomaticTypeAcquisition).
    pub fn is_ata_disabled(&self) -> bool {
        if !self.automatic_type_acquisition_enabled.is_unknown() {
            return !self.automatic_type_acquisition_enabled.is_true();
        }
        self.disable_automatic_type_acquisition.is_true()
    }
}

// userpreferences.go:209
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct InlayHintsPreferences {
    pub include_inlay_parameter_name_hints: IncludeInlayParameterNameHints,
    pub include_inlay_parameter_name_hints_when_argument_matches_name: Tristate,
    pub include_inlay_function_parameter_type_hints: Tristate,
    pub include_inlay_variable_type_hints: Tristate,
    pub include_inlay_variable_type_hints_when_type_matches_name: Tristate,
    pub include_inlay_property_declaration_type_hints: Tristate,
    pub include_inlay_function_like_return_type_hints: Tristate,
    pub include_inlay_enum_member_value_hints: Tristate,
}

// userpreferences.go:220
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct CodeLensUserPreferences {
    pub references_code_lens_enabled: Tristate,
    pub implementations_code_lens_enabled: Tristate,
    pub references_code_lens_show_on_all_functions: Tristate,
    pub implementations_code_lens_show_on_interface_methods: Tristate,
    pub implementations_code_lens_show_on_all_class_methods: Tristate,
}

// --- Enum Types ---

// userpreferences.go:230
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum QuotePreference {
    #[default]
    Unknown,
    Auto,
    Double,
    Single,
}

impl QuotePreference {
    fn as_str(self) -> &'static str {
        match self {
            QuotePreference::Unknown => "",
            QuotePreference::Auto => "auto",
            QuotePreference::Double => "double",
            QuotePreference::Single => "single",
        }
    }
}

// userpreferences.go:232
// Go `type WorkspaceSymbolsScope string`; source12's generated parser accepts the two protocol values.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Default)]
pub struct WorkspaceSymbolsScope(Cow<'static, str>);

impl WorkspaceSymbolsScope {
    const ALL_OPEN_PROJECTS: WorkspaceSymbolsScope =
        WorkspaceSymbolsScope(Cow::Borrowed("allOpenProjects"));
    pub const CURRENT_PROJECT: WorkspaceSymbolsScope =
        WorkspaceSymbolsScope(Cow::Borrowed("currentProject"));
}

// userpreferences.go:246
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum JsxAttributeCompletionStyle {
    #[default]
    Unknown,
    Auto,
    Braces,
    None,
}

impl JsxAttributeCompletionStyle {
    fn as_str(self) -> &'static str {
        match self {
            JsxAttributeCompletionStyle::Unknown => "",
            JsxAttributeCompletionStyle::Auto => "auto",
            JsxAttributeCompletionStyle::Braces => "braces",
            JsxAttributeCompletionStyle::None => "none",
        }
    }
}

// userpreferences.go:255
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum IncludeInlayParameterNameHints {
    #[default]
    None,
    All,
    Literals,
}

impl IncludeInlayParameterNameHints {
    fn as_str(self) -> &'static str {
        match self {
            IncludeInlayParameterNameHints::None => "",
            IncludeInlayParameterNameHints::All => "all",
            IncludeInlayParameterNameHints::Literals => "literals",
        }
    }
}

// userpreferences.go:263
#[repr(i32)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub enum OrganizeImportsSort {
    #[default]
    Auto,
    Ordinal,
    OrdinalIgnoreCase,
    Natural,
    NaturalIgnoreCase,
}

// userpreferences.go:273
// Go `type OrganizeImportsCollation bool`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum OrganizeImportsCollation {
    #[default]
    Ordinal, // false
    Unicode, // true
}

// userpreferences.go:280
#[repr(i32)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub enum OrganizeImportsCaseFirst {
    #[default]
    False = 0,
    Lower = 1,
    Upper = 2,
}

// userpreferences.go:288
#[repr(i32)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub enum OrganizeImportsTypeOrder {
    #[default]
    Auto = 0,
    Last = 1,
    Inline = 2,
    First = 3,
}

// --- Reflection-based parsing infrastructure ---

// userpreferences.go:300
// typeParsers: one function per entry of Go's map, in Go's order; setFieldFromValue dispatches on the field type.
fn parse_tristate_value(val: &Value) -> Tristate {
    if let Value::Bool(b) = val {
        if *b {
            return Tristate::True;
        }
        return Tristate::False;
    }
    Tristate::Unknown
}

fn parse_quote_preference(val: &Value) -> QuotePreference {
    if let Value::String(s) = val {
        match s.to_lowercase().as_str() {
            "auto" => return QuotePreference::Auto,
            "double" => return QuotePreference::Double,
            "single" => return QuotePreference::Single,
            _ => {}
        }
    }
    QuotePreference::Unknown
}

fn parse_jsx_attribute_completion_style(val: &Value) -> JsxAttributeCompletionStyle {
    if let Value::String(s) = val {
        match s.to_lowercase().as_str() {
            "braces" => return JsxAttributeCompletionStyle::Braces,
            "none" => return JsxAttributeCompletionStyle::None,
            _ => {}
        }
    }
    JsxAttributeCompletionStyle::Auto
}

fn parse_include_inlay_parameter_name_hints(val: &Value) -> IncludeInlayParameterNameHints {
    if let Value::String(s) = val {
        match s.to_lowercase().as_str() {
            "all" => return IncludeInlayParameterNameHints::All,
            "literals" => return IncludeInlayParameterNameHints::Literals,
            _ => {}
        }
    }
    IncludeInlayParameterNameHints::None
}

fn parse_organize_imports_sort(val: &Value) -> OrganizeImportsSort {
    if let Value::String(s) = val {
        match s.to_lowercase().as_str() {
            "ordinal" => return OrganizeImportsSort::Ordinal,
            "ordinalignorecase" => return OrganizeImportsSort::OrdinalIgnoreCase,
            "natural" => return OrganizeImportsSort::Natural,
            "naturalignorecase" => return OrganizeImportsSort::NaturalIgnoreCase,
            _ => {}
        }
    }
    OrganizeImportsSort::Auto
}

fn parse_organize_imports_collation(val: &Value) -> OrganizeImportsCollation {
    if let Value::String(s) = val
        && s.to_lowercase() == "unicode"
    {
        return OrganizeImportsCollation::Unicode;
    }
    OrganizeImportsCollation::Ordinal
}

fn parse_organize_imports_case_first(val: &Value) -> OrganizeImportsCaseFirst {
    if let Value::String(s) = val {
        match s.to_lowercase().as_str() {
            "lower" => return OrganizeImportsCaseFirst::Lower,
            "upper" => return OrganizeImportsCaseFirst::Upper,
            _ => {}
        }
    }
    OrganizeImportsCaseFirst::False
}

fn parse_organize_imports_type_order(val: &Value) -> OrganizeImportsTypeOrder {
    if let Value::String(s) = val {
        match s.to_lowercase().as_str() {
            "last" => return OrganizeImportsTypeOrder::Last,
            "inline" => return OrganizeImportsTypeOrder::Inline,
            "first" => return OrganizeImportsTypeOrder::First,
            _ => {}
        }
    }
    OrganizeImportsTypeOrder::Auto
}

fn parse_import_module_specifier_preference(val: &Value) -> ImportModuleSpecifierPreference {
    if let Value::String(s) = val {
        match s.to_lowercase().as_str() {
            "project-relative" => return ImportModuleSpecifierPreference::ProjectRelative,
            "relative" => return ImportModuleSpecifierPreference::Relative,
            "non-relative" => return ImportModuleSpecifierPreference::NonRelative,
            _ => {}
        }
    }
    ImportModuleSpecifierPreference::Shortest
}

fn parse_import_module_specifier_ending_preference(
    val: &Value,
) -> ImportModuleSpecifierEndingPreference {
    if let Value::String(s) = val {
        match s.to_lowercase().as_str() {
            "minimal" => return ImportModuleSpecifierEndingPreference::Minimal,
            "index" => return ImportModuleSpecifierEndingPreference::Index,
            "js" => return ImportModuleSpecifierEndingPreference::Js,
            _ => {}
        }
    }
    ImportModuleSpecifierEndingPreference::Auto
}

fn parse_workspace_symbols_scope(val: &Value) -> WorkspaceSymbolsScope {
    if let Value::String(s) = val {
        match s.to_lowercase().as_str() {
            "allopenprojects" => return WorkspaceSymbolsScope::ALL_OPEN_PROJECTS,
            "currentproject" => return WorkspaceSymbolsScope::CURRENT_PROJECT,
            _ => {}
        }
    }
    WorkspaceSymbolsScope::default()
}

// userpreferences.go:426
// typeSerializers maps a field type to a function that serializes a value of that type.
// For types which do not serialize as-is (tristate, enums, etc). Returns None for a type without an entry; the inner
// None is Go's nil.
#[expect(
    clippy::option_option,
    reason = "Distinguishes no matching serializer or field from a matching nil value, as in Go."
)]
fn type_serializers(field: &FieldRef<'_>) -> Option<Option<Value>> {
    Some(match field {
        FieldRef::Tristate(v) => match **v {
            Tristate::True => Some(Value::Bool(true)),
            Tristate::False => Some(Value::Bool(false)),
            Tristate::Unknown => None,
        },
        FieldRef::OrganizeImportsSort(v) => Some(Value::String(
            match **v {
                OrganizeImportsSort::Ordinal => "ordinal",
                OrganizeImportsSort::OrdinalIgnoreCase => "ordinalIgnoreCase",
                OrganizeImportsSort::Natural => "natural",
                OrganizeImportsSort::NaturalIgnoreCase => "naturalIgnoreCase",
                OrganizeImportsSort::Auto => "auto",
            }
            .to_string(),
        )),
        FieldRef::OrganizeImportsCollation(v) => {
            if **v == OrganizeImportsCollation::Unicode {
                Some(Value::String("unicode".to_string()))
            } else {
                Some(Value::String("ordinal".to_string()))
            }
        }
        FieldRef::OrganizeImportsCaseFirst(v) => Some(Value::String(
            match **v {
                OrganizeImportsCaseFirst::Lower => "lower",
                OrganizeImportsCaseFirst::Upper => "upper",
                OrganizeImportsCaseFirst::False => "default",
            }
            .to_string(),
        )),
        FieldRef::OrganizeImportsTypeOrder(v) => Some(Value::String(
            match **v {
                OrganizeImportsTypeOrder::Last => "last",
                OrganizeImportsTypeOrder::Inline => "inline",
                OrganizeImportsTypeOrder::First => "first",
                OrganizeImportsTypeOrder::Auto => "auto",
            }
            .to_string(),
        )),
        // These enums distinguish an unset zero value (e.g. "") from their effective
        // default (e.g. "auto"): the parser promotes unset/unknown input to the
        // non-zero default. Plain string serialization would therefore write "" for
        // an unset field and the parser would read it back as the non-zero default,
        // breaking round-tripping. Mirror the core.Tristate serializer above and omit
        // the unset value (return nil) so it decodes back to the zero value. (Enums
        // whose default already is their zero value, like the OrganizeImports* ones,
        // round-trip without this.)
        //
        // TODO: These three are the only parsers whose fallback is a non-zero value;
        // every other parser returns its zero value as the fallback. They should be
        // made consistent: change the parser fallback to return the zero value and
        // remove this serializer (relying on the default string serialization, which
        // already omits ""). The consumer must then treat the zero value as the
        // effective default. The two module-specifier enums are safe to convert (all
        // read sites already treat the "" zero identically to the promoted default).
        FieldRef::JsxAttributeCompletionStyle(v) => {
            // TODO: make consistent with other enums (see note above). Unlike the
            // module-specifier enums, the consumer in completions.go distinguishes
            // JsxAttributeCompletionStyleUnknown from ...Auto, so converting this one
            // requires updating that consumer to treat the zero value as "auto".
            if **v == JsxAttributeCompletionStyle::Unknown {
                None
            } else {
                Some(Value::String(v.as_str().to_string()))
            }
        }
        FieldRef::ImportModuleSpecifierPreference(v) => {
            // TODO: make consistent with other enums (see note above): have the parser
            // return the zero value (None) as its fallback and drop this serializer.
            if v.as_str().is_empty() { None } else { Some(Value::String(v.as_str().to_string())) }
        }
        FieldRef::ImportModuleSpecifierEndingPreference(v) => {
            // TODO: make consistent with other enums (see note above): have the parser
            // return the zero value (None) as its fallback and drop this serializer.
            if v.as_str().is_empty() { None } else { Some(Value::String(v.as_str().to_string())) }
        }
        _ => return None,
    })
}

// userpreferences.go:525
// configPathParsers provides field-specific config value parsers that override the default
// type-based parser when the VS Code config value format differs from the Go field type.
fn config_path_parsers(path: &str) -> Option<fn(&Value) -> Tristate> {
    match path {
        // VS Code sends caseSensitivity as a string ("auto"/"caseSensitive"/"caseInsensitive"),
        // but OrganizeImportsIgnoreCase is a core.Tristate.
        "preferences.organizeImports.caseSensitivity" => Some(|val: &Value| {
            if let Value::String(s) = val {
                match s.to_lowercase().as_str() {
                    "caseinsensitive" => return Tristate::True,
                    "casesensitive" => return Tristate::False,
                    _ => {}
                }
            }
            if let Value::Bool(b) = val {
                if *b {
                    return Tristate::True;
                }
                return Tristate::False;
            }
            Tristate::Unknown
        }),
        _ => None,
    }
}

// userpreferences.go:547
pub struct FieldInfo {
    pub raw_name: &'static str, // raw name for unstable section lookup (e.g., "quotePreference")
    config_path: &'static str,  // dotted path for config (e.g., "preferences.quoteStyle")
    fallback_config_paths: Vec<ConfigPathInfo>,
    field: &'static FieldTag, // Go: fieldPath, the index path to the field in the struct
    raw_invert: bool,         // whether to invert boolean values for raw name
    config_invert: bool,      // whether to invert boolean values for config path
}

// userpreferences.go:556
#[derive(Clone, Copy)]
pub struct ConfigPathInfo {
    path: &'static str,
    invert: bool,
}

// userpreferences.go:561
pub fn field_info_cache() -> &'static [FieldInfo] {
    static CACHE: OnceLock<Vec<FieldInfo>> = OnceLock::new();
    CACHE.get_or_init(|| collect_field_infos(USER_PREFERENCES_FIELD_TAGS))
}

// userpreferences.go:566
// unstableNameIndex maps raw names to fieldInfo index for unstable section lookup.
pub fn unstable_name_index() -> &'static FxHashMap<&'static str, usize> {
    static INDEX: OnceLock<FxHashMap<&'static str, usize>> = OnceLock::new();
    INDEX.get_or_init(|| {
        let infos = field_info_cache();
        let mut index = FxHashMap::with_capacity_and_hasher(infos.len(), rustc_hash::FxBuildHasher);
        index.extend(infos.iter().enumerate().filter_map(|(i, info)| {
            if info.raw_name.is_empty() { None } else { Some((info.raw_name, i)) }
        }));
        index
    })
}

// userpreferences.go:577
fn collect_field_infos(fields: &'static [FieldTag]) -> Vec<FieldInfo> {
    let mut infos = Vec::new();
    for field in fields {
        let raw_tag = field.raw;
        let config_tag = field.config;
        let fallback_config_tag = field.fallback_config;

        // Go recurses into embedded structs without tags here; the table is already flattened.
        assert!(
            !(raw_tag.is_empty() && config_tag.is_empty()),
            "raw or config tag required for field {}",
            field.name
        );

        let mut info = FieldInfo {
            raw_name: "",
            config_path: "",
            fallback_config_paths: Vec::new(),
            field,
            raw_invert: false,
            config_invert: false,
        };

        // Parse raw tag: "name" or "name,invert"
        if !raw_tag.is_empty() {
            let mut parts = raw_tag.split(',');
            info.raw_name = parts.next().unwrap();
            for part in parts {
                if part == "invert" {
                    info.raw_invert = true;
                }
            }
        }

        // Parse config tag: "path.to.setting" or "path.to.setting,invert"
        if !config_tag.is_empty() {
            let config_path = parse_config_path_tag(config_tag);
            info.config_path = config_path.path;
            info.config_invert = config_path.invert;
        }
        if !fallback_config_tag.is_empty() {
            info.fallback_config_paths
                .extend(fallback_config_tag.split(';').map(parse_config_path_tag));
        }

        infos.push(info);
    }
    infos
}

// userpreferences.go:628
fn parse_config_path_tag(tag: &'static str) -> ConfigPathInfo {
    let mut parts = tag.split(',');
    let mut info = ConfigPathInfo { path: parts.next().unwrap(), invert: false };
    for part in parts {
        if part == "invert" {
            info.invert = true;
        }
    }
    info
}

// userpreferences.go:639
fn get_nested_value<'a>(config: &'a JsonMap, path: &str) -> Option<&'a Value> {
    let mut parts = path.split('.');
    let mut current = config.get(parts.next().unwrap())?;
    current = parts.try_fold(current, |current, part| {
        let Value::Object(m) = current else {
            return None;
        };
        m.get(part)
    })?;
    Some(current)
}

// userpreferences.go:655
fn set_nested_value(config: &mut JsonMap, path: &str, value: Value) {
    let parts: Vec<&str> = path.split('.').collect();
    let mut current = config;
    current = parts[..parts.len() - 1].iter().fold(current, |mut current, part| {
        if !matches!(current.get(part), Some(Value::Object(_))) {
            current.insert(part.to_string(), Value::Object(JsonMap::new()));
        }
        let Some(Value::Object(next)) = current.get_mut(part) else { unreachable!() };
        current = next;

        current
    });
    current.insert(parts[parts.len() - 1].to_string(), value);
}

// userpreferences.go:669
fn set_raw_fields_from_config(p: &mut UserPreferences, infos: &[FieldInfo], settings: &JsonMap) {
    let index = unstable_name_index();
    for (name, value) in settings {
        if let Some(&idx) = index.get(name) {
            let info = &infos[idx];
            let field = (info.field.get_mut)(p);
            let inverted;
            let mut value = value;
            if info.raw_invert
                && let Value::Bool(b) = value
            {
                inverted = Value::Bool(!*b);
                value = &inverted;
            }
            set_field_from_value(field, value);
        }
    }
}

impl UserPreferences {
    // userpreferences.go:685
    #[must_use]
    pub fn with_config(mut self, config: &JsonMap) -> UserPreferences {
        let p = &mut self;
        let infos = field_info_cache();

        // Raw UserPreferences can be provided directly, notably via LSP initializationOptions.
        set_raw_fields_from_config(p, infos, config);

        // Process "unstable" section first - allows any field to be set by raw name.
        // This mirrors VS Code's behavior: { ...config.get('unstable'), ...stableOptions }
        // where stable options are spread after and take precedence.
        if let Some(Value::Object(unstable)) = config.get("unstable") {
            set_raw_fields_from_config(p, infos, unstable);
        }

        // Process path-based config (VS Code style nested paths).
        // These run after unstable, so stable config values take precedence.
        for info in infos {
            if info.config_path.is_empty() {
                continue;
            }
            let mut config_path =
                ConfigPathInfo { path: info.config_path, invert: info.config_invert };
            let mut val = get_nested_value(config, config_path.path);
            if val.is_none() {
                for fallback_config_path in &info.fallback_config_paths {
                    val = get_nested_value(config, fallback_config_path.path);
                    if val.is_some() {
                        config_path = *fallback_config_path;
                        break;
                    }
                }
            }
            let Some(mut val) = val else {
                continue;
            };

            let field = (info.field.get_mut)(p);
            let inverted;
            if config_path.invert
                && let Value::Bool(b) = val
            {
                inverted = Value::Bool(!*b);
                val = &inverted;
            }
            if let Some(parser) = config_path_parsers(config_path.path) {
                match field {
                    FieldMut::Tristate(f) => *f = parser(val),
                    _ => panic!(
                        "reflect.Set: value of type core.Tristate is not assignable to field {}",
                        info.field.name
                    ),
                }
                continue;
            }
            set_field_from_value(field, val);
        }

        // Validate CustomConfigFileName for path traversal
        if !p.custom_config_file_name.is_empty() {
            let name = p.custom_config_file_name.trim();
            if name.contains(['/', '\\']) || name == ".." || name == "." {
                p.custom_config_file_name = String::new();
            } else {
                p.custom_config_file_name = name.to_string();
            }
        }

        self
    }
}

// userpreferences.go:753
fn set_field_from_value(field: FieldMut<'_>, val: &Value) {
    if matches!(val, Value::Null) {
        return;
    }

    match field {
        // Check custom parsers first (for types like Tristate, enums, etc.)
        FieldMut::Tristate(f) => *f = parse_tristate_value(val),
        FieldMut::IndentStyle(f) => *f = parse_indent_style(val),
        FieldMut::SemicolonPreference(f) => *f = parse_semicolon_preference(val),
        FieldMut::QuotePreference(f) => *f = parse_quote_preference(val),
        FieldMut::JsxAttributeCompletionStyle(f) => *f = parse_jsx_attribute_completion_style(val),
        FieldMut::IncludeInlayParameterNameHints(f) => {
            *f = parse_include_inlay_parameter_name_hints(val);
        }
        FieldMut::OrganizeImportsSort(f) => *f = parse_organize_imports_sort(val),
        FieldMut::OrganizeImportsCollation(f) => *f = parse_organize_imports_collation(val),
        FieldMut::OrganizeImportsCaseFirst(f) => *f = parse_organize_imports_case_first(val),
        FieldMut::OrganizeImportsTypeOrder(f) => *f = parse_organize_imports_type_order(val),
        FieldMut::ImportModuleSpecifierPreference(f) => {
            *f = parse_import_module_specifier_preference(val);
        }
        FieldMut::ImportModuleSpecifierEndingPreference(f) => {
            *f = parse_import_module_specifier_ending_preference(val);
        }

        FieldMut::Bool(f) => {
            if let Value::Bool(b) = val {
                *f = *b;
            }
        }
        FieldMut::Int(f) => {
            // Go accepts int and float64; JSON numbers are float64.
            if let Value::Number(v) = val {
                *f = *v as i64 as i32;
            }
        }
        FieldMut::String(f) => {
            if let Value::String(s) = val {
                f.clone_from(s);
            }
        }
        FieldMut::WorkspaceSymbolsScope(f) => {
            *f = parse_workspace_symbols_scope(val);
        }
        FieldMut::Slice(f) => {
            if let Value::Array(arr) = val {
                let mut result = Vec::with_capacity(arr.len());
                result.extend(arr.iter().filter_map(|item| {
                    if let Value::String(s) = item { Some(s.clone()) } else { None }
                }));
                *f = Some(result);
            }
        }
    }
}

impl UserPreferences {
    // userpreferences.go:793
    // Returns the JSON value Go's encoder writes (json.Deterministic(true): object keys sorted).
    pub fn marshal_json_to(&self) -> Value {
        let mut config = JsonMap::new();

        for info in field_info_cache() {
            let field = (info.field.get)(self);

            let Some(mut val) = serialize_field(field) else {
                continue;
            };

            // Prefer config path if available, otherwise use unstable section
            if !info.config_path.is_empty() {
                if info.config_invert
                    && let Value::Bool(b) = val
                {
                    val = Value::Bool(!b);
                }
                set_nested_value(&mut config, info.config_path, val);
            } else if !info.raw_name.is_empty() {
                if info.raw_invert
                    && let Value::Bool(b) = val
                {
                    val = Value::Bool(!b);
                }
                set_nested_value(&mut config, &format!("unstable.{}", info.raw_name), val);
            }
        }

        let mut config = Value::Object(config);
        sort_object_keys(&mut config);
        config
    }
}

fn sort_object_keys(v: &mut Value) {
    if let Value::Object(m) = v {
        m.sort_keys();
        for (_, item) in m.iter_mut() {
            sort_object_keys(item);
        }
    }
}

// userpreferences.go:826
fn serialize_field(field: FieldRef<'_>) -> Option<Value> {
    // Check custom serializers first (for types like Tristate, enums, etc.)
    if let Some(serialized) = type_serializers(&field) {
        return serialized;
    }

    match field {
        FieldRef::Bool(b) => Some(Value::Bool(*b)),
        // Go reflect.Int kind.
        FieldRef::Int(i) => serialize_int(*i),
        FieldRef::IndentStyle(i) => serialize_int(i.0),
        // Zero ("") means "unset"; omit it for the same reason as int above.
        FieldRef::String(s) => serialize_string(s),
        FieldRef::SemicolonPreference(s) => serialize_string(s.as_str()),
        FieldRef::QuotePreference(s) => serialize_string(s.as_str()),
        FieldRef::IncludeInlayParameterNameHints(s) => serialize_string(s.as_str()),
        FieldRef::WorkspaceSymbolsScope(s) => serialize_string(&s.0),
        FieldRef::Slice(v) => {
            let v = v.as_ref()?;
            Some(Value::Array(v.iter().map(|s| Value::String(s.clone())).collect()))
        }
        _ => unreachable!("field type has a serializer"),
    }
}

fn serialize_int(i: i32) -> Option<Value> {
    // Zero means "unset" for these preference fields. Omit it so a partial
    // config does not clobber defaults with zeros when round-tripped through
    // withConfig.
    if i == 0 {
        return None;
    }
    Some(Value::Number(i as f64))
}

fn serialize_string(s: &str) -> Option<Value> {
    if s.is_empty() {
        return None;
    }
    Some(Value::String(s.to_string()))
}

impl UserPreferences {
    // userpreferences.go:865
    pub fn unmarshal_json_from(&mut self, value: &Value) -> Result<(), String> {
        let empty = JsonMap::new();
        let config = match value {
            Value::Object(m) => m,
            // A JSON null leaves Go's map nil.
            Value::Null => &empty,
            _ => {
                return Err(
                    "json: cannot unmarshal into Go value of type map[string]any".to_string()
                );
            }
        };
        // Start with defaults, then overlay parsed values
        *self = new_default_user_preferences().with_config(config);
        Ok(())
    }

    // --- Helper methods ---

    // userpreferences.go:877
    pub fn module_specifier_preferences(&self) -> tsrs_modulespecifiers::UserPreferences {
        tsrs_modulespecifiers::UserPreferences {
            import_module_specifier_preference: self.import_module_specifier_preference,
            import_module_specifier_ending: self.import_module_specifier_ending,
            auto_import_specifier_exclude_regexes: self
                .auto_import_specifier_exclude_regexes
                .clone()
                .unwrap_or_default(),
        }
    }

    // userpreferences.go:885
    pub(crate) fn parsed_auto_import_file_exclude_patterns(
        &self,
        use_case_sensitive_file_names: bool,
    ) -> Option<vfsmatch::SpecMatcher> {
        vfsmatch::new_spec_matcher(
            self.auto_import_file_exclude_patterns.as_deref().unwrap_or(&[]),
            "",
            vfsmatch::Usage::Exclude,
            use_case_sensitive_file_names,
        )
    }
}

// userpreferences.go:893
pub fn parse_user_preferences(items: &JsonMap) -> UserPreferences {
    let mut prefs = new_default_user_preferences();
    // Apply editor settings first (tabSize, indentSize, etc.) as raw-name defaults,
    // then overlay language-specific settings with increasing precedence:
    // editor < javascript < typescript < js/ts
    if let Some(Value::Object(editor_settings)) = items.get("editor") {
        let mut normalized_settings = editor_settings.clone();
        if let Some(tab_size) = normalized_settings.get("tabSize").cloned()
            && !normalized_settings.contains_key("indentSize")
        {
            normalized_settings.insert("indentSize".to_string(), tab_size);
        }
        if let Some(insert_spaces) = normalized_settings.get("insertSpaces").cloned()
            && !normalized_settings.contains_key("convertTabsToSpaces")
        {
            normalized_settings.insert("convertTabsToSpaces".to_string(), insert_spaces);
        }
        let mut unstable = JsonMap::new();
        unstable.insert("unstable".to_string(), Value::Object(normalized_settings));
        prefs = prefs.with_config(&unstable);
    }
    // Apply javascript, then typescript, then js/ts (highest precedence).
    prefs =
        (["javascript", "typescript", "js/ts"]).into_iter().fold(prefs, |mut prefs, section| {
            if let Some(Value::Object(settings)) = items.get(section) {
                prefs = prefs.with_config(settings);
            }

            prefs
        });
    prefs
}

/// Decode the API's `userPreferences` object, starting from Go's default preferences.
/// Unlike LSP configuration, the API value is itself a flat/raw preference object.
pub fn parse_api_user_preferences(config: &JsonMap) -> UserPreferences {
    new_default_user_preferences().with_config(config)
}
