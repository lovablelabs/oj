use crate::*;
use tsrs_ast::*;
use tsrs_core::tspath;
use tsrs_core::*;

// Non-function declarations in preferences.go (hand-ported in types.rs):
//   type ModuleSpecifierPreferences (preferences.go:141)

// Program errors validate that `noEmit` or `emitDeclarationOnly` is also set,
// so this function doesn't check them to avoid propagating errors.
// preferences.go:14
fn should_allow_importing_ts_extension(
    compiler_options: &CompilerOptions,
    from_file_name: &str,
) -> bool {
    compiler_options.get_allow_importing_ts_extensions()
        || !from_file_name.is_empty() && tspath::is_declaration_file_name(from_file_name)
}

// preferences.go:18
fn uses_extensions_on_imports(file: P<SourceFile>) -> bool {
    if let Some(result) = file.imports().iter().find_map(|ref_| {
        let text = ref_.text();
        if tspath::path_is_relative(text)
            && !tspath::file_extension_is_one_of(
                text,
                tspath::EXTENSIONS_NOT_SUPPORTING_EXTENSIONLESS_RESOLUTION,
            )
        {
            return Some(
                tspath::has_ts_file_extension(text) || tspath::has_js_file_extension(text),
            );
        }

        None
    }) {
        return result;
    }
    false
}

// preferences.go:28
fn infer_preference(
    resolution_mode: ModuleKind,
    source_file: Option<P<SourceFile>>,
    module_resolution_is_node_next: bool,
) -> ModuleSpecifierEnding {
    let mut uses_js_extensions = false;
    let mut specifiers: &[P<Node>] = &[];
    if let Some(source_file) = source_file.filter(|f| !f.imports().is_empty()) {
        specifiers = source_file.imports();
    } else if source_file.is_some_and(|f| f.is_js()) {
        // !!! TODO: JS support
        // specifiers = core.Map(getRequiresAtTopOfFile(sourceFile), func(d *ast.Node) *ast.Node { return d.arguments[0] })
    }

    for specifier in specifiers {
        let path = specifier.text();
        if tspath::path_is_relative(path) {
            // !!! TODO: proper resolutionMode support
            if module_resolution_is_node_next && resolution_mode == RESOLUTION_MODE_COMMON_JS
            /* && getModeForUsageLocation(sourceFile!, specifier, compilerOptions) === ModuleKind.ESNext */
            {
                // We're trying to decide a preference for a CommonJS module specifier, but looking at an ESM import.
                continue;
            }
            if tspath::file_extension_is_one_of(
                path,
                tspath::EXTENSIONS_NOT_SUPPORTING_EXTENSIONLESS_RESOLUTION,
            ) {
                // These extensions are not optional, so do not indicate a preference.
                continue;
            }
            if tspath::has_ts_file_extension(path) {
                return ModuleSpecifierEnding::TsExtension;
            }
            if tspath::has_js_file_extension(path) {
                uses_js_extensions = true;
            }
        }
    }

    if uses_js_extensions {
        return ModuleSpecifierEnding::JsExtension;
    }
    ModuleSpecifierEnding::Minimal
}

fn module_resolution_is_node_next(compiler_options: &CompilerOptions) -> bool {
    let module_resolution = compiler_options.get_module_resolution_kind();
    ModuleResolutionKind::NODE16 <= module_resolution
        && module_resolution <= ModuleResolutionKind::NODE_NEXT
}

// preferences.go:69
fn get_module_specifier_ending_preference(
    pref: ImportModuleSpecifierEndingPreference,
    resolution_mode: ModuleKind,
    compiler_options: &CompilerOptions,
    source_file: Option<P<SourceFile>>,
) -> ModuleSpecifierEnding {
    let module_resolution_is_node_next = module_resolution_is_node_next(compiler_options);

    if pref == ImportModuleSpecifierEndingPreference::Js
        || resolution_mode == RESOLUTION_MODE_ESM && module_resolution_is_node_next
    {
        // Extensions are explicitly requested or required. Now choose between .js and .ts.
        if !should_allow_importing_ts_extension(compiler_options, "") {
            return ModuleSpecifierEnding::JsExtension;
        }
        // `allowImportingTsExtensions` is a strong signal, so use .ts unless the file
        // already uses .js extensions and no .ts extensions.
        if infer_preference(resolution_mode, source_file, module_resolution_is_node_next)
            != ModuleSpecifierEnding::JsExtension
        {
            return ModuleSpecifierEnding::TsExtension;
        }
        return ModuleSpecifierEnding::JsExtension;
    }

    if pref == ImportModuleSpecifierEndingPreference::Minimal {
        return ModuleSpecifierEnding::Minimal;
    }

    if pref == ImportModuleSpecifierEndingPreference::Index {
        return ModuleSpecifierEnding::Index;
    }

    // No preference was specified.
    // Look at imports and/or requires to guess whether .js, .ts, or extensionless imports are preferred.
    // N.B. that `Index` detection is not supported since it would require file system probing to do
    // accurately, and more importantly, literally nobody wants `Index` and its existence is a mystery.
    if !should_allow_importing_ts_extension(compiler_options, "") {
        // If .ts imports are not valid, we only need to see one .js import to go with that.
        if source_file.is_some_and(uses_extensions_on_imports) {
            return ModuleSpecifierEnding::JsExtension;
        }
        return ModuleSpecifierEnding::Minimal;
    }

    infer_preference(resolution_mode, source_file, module_resolution_is_node_next)
}

// preferences.go:114
fn get_preferred_ending(
    prefs: &UserPreferences,
    host: &dyn ModuleSpecifierGenerationHost,
    compiler_options: &CompilerOptions,
    importing_source_file: P<SourceFile>,
    old_import_specifier: &str,
    resolution_mode: ModuleKind,
) -> ModuleSpecifierEnding {
    let mut resolution_mode = resolution_mode;
    if !old_import_specifier.is_empty() {
        if tspath::has_js_file_extension(old_import_specifier) {
            return ModuleSpecifierEnding::JsExtension;
        }
        if old_import_specifier.ends_with("/index") {
            return ModuleSpecifierEnding::Index;
        }
    }
    if resolution_mode == RESOLUTION_MODE_NONE {
        resolution_mode = host.get_default_resolution_mode_for_file(importing_source_file);
    }
    get_module_specifier_ending_preference(
        prefs.import_module_specifier_ending,
        resolution_mode,
        compiler_options,
        Some(importing_source_file),
    )
}

// preferences.go:147
pub fn get_allowed_endings_in_preferred_order(
    prefs: &UserPreferences,
    host: &dyn ModuleSpecifierGenerationHost,
    compiler_options: &CompilerOptions,
    importing_source_file: P<SourceFile>,
    old_import_specifier: &str,
    syntax_implied_node_format: ModuleKind,
) -> Vec<ModuleSpecifierEnding> {
    let mut preferred_ending = get_preferred_ending(
        prefs,
        host,
        compiler_options,
        importing_source_file,
        old_import_specifier,
        RESOLUTION_MODE_NONE,
    );
    let resolution_mode = host.get_default_resolution_mode_for_file(importing_source_file);
    if resolution_mode != syntax_implied_node_format {
        preferred_ending = get_preferred_ending(
            prefs,
            host,
            compiler_options,
            importing_source_file,
            old_import_specifier,
            syntax_implied_node_format,
        );
    }
    let module_resolution_is_node_next = module_resolution_is_node_next(compiler_options);
    let allow_importing_ts_extension =
        should_allow_importing_ts_extension(compiler_options, importing_source_file.file_name());
    // TypeScript uses `(syntaxImpliedNodeFormat ?? impliedNodeFormat)` here - fall back to the
    // file's default resolution mode when no syntax-implied mode is given.
    let mut effective_syntax_mode = syntax_implied_node_format;
    if effective_syntax_mode == RESOLUTION_MODE_NONE {
        effective_syntax_mode = resolution_mode;
    }
    if effective_syntax_mode == RESOLUTION_MODE_ESM && module_resolution_is_node_next {
        if allow_importing_ts_extension {
            return vec![ModuleSpecifierEnding::TsExtension, ModuleSpecifierEnding::JsExtension];
        }
        return vec![ModuleSpecifierEnding::JsExtension];
    }
    match preferred_ending {
        ModuleSpecifierEnding::JsExtension => {
            if allow_importing_ts_extension {
                return vec![
                    ModuleSpecifierEnding::JsExtension,
                    ModuleSpecifierEnding::TsExtension,
                    ModuleSpecifierEnding::Minimal,
                    ModuleSpecifierEnding::Index,
                ];
            }
            vec![
                ModuleSpecifierEnding::JsExtension,
                ModuleSpecifierEnding::Minimal,
                ModuleSpecifierEnding::Index,
            ]
        }
        ModuleSpecifierEnding::TsExtension => vec![
            ModuleSpecifierEnding::TsExtension,
            ModuleSpecifierEnding::Minimal,
            ModuleSpecifierEnding::JsExtension,
            ModuleSpecifierEnding::Index,
        ],
        ModuleSpecifierEnding::Index => {
            if allow_importing_ts_extension {
                return vec![
                    ModuleSpecifierEnding::Index,
                    ModuleSpecifierEnding::Minimal,
                    ModuleSpecifierEnding::TsExtension,
                    ModuleSpecifierEnding::JsExtension,
                ];
            }
            vec![
                ModuleSpecifierEnding::Index,
                ModuleSpecifierEnding::Minimal,
                ModuleSpecifierEnding::JsExtension,
            ]
        }
        ModuleSpecifierEnding::Minimal => {
            if allow_importing_ts_extension {
                return vec![
                    ModuleSpecifierEnding::Minimal,
                    ModuleSpecifierEnding::Index,
                    ModuleSpecifierEnding::TsExtension,
                    ModuleSpecifierEnding::JsExtension,
                ];
            }
            vec![
                ModuleSpecifierEnding::Minimal,
                ModuleSpecifierEnding::Index,
                ModuleSpecifierEnding::JsExtension,
            ]
        }
    }
}

// preferences.go:213
#[expect(
    clippy::redundant_pub_crate,
    reason = "Only the public API is re-exported; implementation helpers remain crate-private."
)]
pub(crate) fn get_module_specifier_preferences(
    prefs: &UserPreferences,
    _host: &dyn ModuleSpecifierGenerationHost,
    _compiler_options: &CompilerOptions,
    importing_source_file: P<SourceFile>,
    old_import_specifier: &str,
) -> ModuleSpecifierPreferences {
    let excludes = prefs.auto_import_specifier_exclude_regexes.clone();
    let mut relative_preference = RelativePreferenceKind::Shortest;
    if old_import_specifier.is_empty() {
        match prefs.import_module_specifier_preference {
            ImportModuleSpecifierPreference::Relative => {
                relative_preference = RelativePreferenceKind::Relative;
            }
            ImportModuleSpecifierPreference::NonRelative => {
                relative_preference = RelativePreferenceKind::NonRelative;
            }
            ImportModuleSpecifierPreference::ProjectRelative => {
                relative_preference = RelativePreferenceKind::ExternalNonRelative;
            }
            // all others are shortest
            _ => {}
        }
    } else if tspath::is_external_module_name_relative(old_import_specifier) {
        relative_preference = RelativePreferenceKind::Relative;
    } else {
        relative_preference = RelativePreferenceKind::NonRelative;
    }

    // Go's `getAllowedEndingsInPreferredOrder` closure: see `ModuleSpecifierPreferences` (types.rs).
    ModuleSpecifierPreferences {
        exclude_regexes: excludes,
        relative_preference,
        prefs: prefs.clone(),
        importing_source_file,
        old_import_specifier: old_import_specifier.to_string(),
    }
}

impl ModuleSpecifierPreferences {
    /// Go `preferences.getAllowedEndingsInPreferredOrder(syntaxImpliedNodeFormat)`; `host` and `compiler_options` are
    /// the ones `get_module_specifier_preferences` was called with.
    pub(crate) fn get_allowed_endings_in_preferred_order(
        &self,
        host: &dyn ModuleSpecifierGenerationHost,
        compiler_options: &CompilerOptions,
        syntax_implied_node_format: ModuleKind,
    ) -> Vec<ModuleSpecifierEnding> {
        get_allowed_endings_in_preferred_order(
            &self.prefs,
            host,
            compiler_options,
            self.importing_source_file,
            &self.old_import_specifier,
            syntax_implied_node_format,
        )
    }
}
