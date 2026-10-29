use crate::*;
use rustc_hash::FxHashMap;
use tsrs_ast as ast;
use tsrs_ast::*;
use tsrs_core::stringutil;
use tsrs_core::tspath;
use tsrs_core::*;
use tsrs_module::packagejson::{self, JSONData, JSONValueType};
use tsrs_tsoptions::outputpaths;

// Non-function declarations in specifiers.go (hand-ported in types.rs):
//   type ambientModuleInfo (specifiers.go:107)
//   type Info (specifiers.go:162)
//   type pkgJsonDirAttemptResult (specifiers.go:830)
//   type specPair (specifiers.go:1089)

// specifiers.go:19
pub fn get_module_specifiers(
    module_symbol: P<Symbol>,
    checker: &mut dyn CheckerShape,
    compiler_options: &CompilerOptions,
    importing_source_file: P<SourceFile>,
    host: &dyn ModuleSpecifierGenerationHost,
    user_preferences: &UserPreferences,
    options: ModuleSpecifierOptions,
    for_auto_imports: bool,
) -> ModuleSpecifiersResult {
    get_module_specifiers_with_info(
        module_symbol,
        checker,
        compiler_options,
        importing_source_file,
        host,
        user_preferences,
        options,
        for_auto_imports,
    )
}

// specifiers.go:41
fn get_module_specifiers_with_info(
    module_symbol: P<Symbol>,
    checker: &mut dyn CheckerShape,
    compiler_options: &CompilerOptions,
    importing_source_file: P<SourceFile>,
    host: &dyn ModuleSpecifierGenerationHost,
    user_preferences: &UserPreferences,
    options: ModuleSpecifierOptions,
    for_auto_imports: bool,
) -> ModuleSpecifiersResult {
    let ambient = try_get_module_name_from_ambient_module(module_symbol, checker);
    if !ambient.name.is_empty() {
        if for_auto_imports
            && is_excluded_by_regex(
                &ambient.name,
                &user_preferences.auto_import_specifier_exclude_regexes,
            )
        {
            return ModuleSpecifiersResult {
                specifiers: Vec::new(),
                ambient_module_symbol: ambient.symbol,
            };
        }
        return ModuleSpecifiersResult {
            specifiers: vec![ambient.name],
            ambient_module_symbol: ambient.symbol,
        };
    }

    let Some(module_source_file) = ast::get_source_file_of_module(module_symbol) else {
        return ModuleSpecifiersResult::default();
    };

    // Use original source file name when file is from project reference output
    let module_file_name =
        host.get_source_of_project_reference_if_output_included(module_source_file);

    let (specifiers, _kind) = get_module_specifiers_for_file_with_info(
        importing_source_file,
        &module_file_name,
        compiler_options,
        host,
        user_preferences,
        options,
        for_auto_imports,
    );
    ModuleSpecifiersResult { specifiers, ambient_module_symbol: None }
}

// specifiers.go:79
pub fn get_module_specifiers_for_file_with_info(
    importing_source_file: P<SourceFile>,
    module_file_name: &str,
    compiler_options: &CompilerOptions,
    host: &dyn ModuleSpecifierGenerationHost,
    user_preferences: &UserPreferences,
    options: ModuleSpecifierOptions,
    for_auto_imports: bool,
) -> (Vec<String>, ResultKind) {
    let module_paths = get_all_module_paths_worker(
        &get_info(
            &host.get_source_of_project_reference_if_output_included(importing_source_file),
            host,
        ),
        module_file_name,
        host,
        compiler_options,
        options,
    );

    compute_module_specifiers(
        &module_paths,
        compiler_options,
        importing_source_file,
        host,
        user_preferences,
        options,
        for_auto_imports,
    )
}

// specifiers.go:112
fn try_get_module_name_from_ambient_module(
    module_symbol: P<Symbol>,
    checker: &mut dyn CheckerShape,
) -> AmbientModuleInfo {
    let declarations = module_symbol.declarations();
    if let Some(result) = declarations.iter().find_map(|&decl| {
        if ast::is_module_with_string_literal_name(decl)
            && (!ast::is_module_augmentation_external(decl)
                || !tspath::is_external_module_name_relative(decl.name().unwrap().text()))
        {
            Some(AmbientModuleInfo {
                name: decl.name().unwrap().text().to_string(),
                symbol: Some(module_symbol),
            })
        } else {
            None
        }
    }) {
        return result;
    }

    // the module could be a namespace, which is export through "export=" from an ambient module.
    /*
     * declare module "m" {
     *     namespace ns {
     *         class c {}
     *     }
     *     export = ns;
     * }
     */
    // `import {c} from "m";` is valid, in which case, `moduleSymbol` is "ns", but the module name should be "m"
    for &d in declarations {
        if !ast::is_module_declaration(d) {
            continue;
        }

        let possible_container = ast::find_ancestor(d, ast::is_module_with_string_literal_name);
        let Some(possible_container) = possible_container else {
            continue;
        };
        if !possible_container.parent().is_some_and(ast::is_source_file) {
            continue;
        }

        let sym = possible_container
            .symbol()
            .unwrap()
            .exports()
            .and_then(|exports| exports.lookup(ast::INTERNAL_SYMBOL_NAME_EXPORT_EQUALS));
        let Some(sym) = sym else {
            continue;
        };
        let export_assignment_decl = sym.value_declaration();
        let Some(export_assignment_decl) =
            export_assignment_decl.filter(|decl| decl.kind() == Kind::ExportAssignment)
        else {
            continue;
        };
        let export_symbol =
            checker.get_symbol_at_location(export_assignment_decl.expression().unwrap());
        let Some(mut export_symbol) = export_symbol else {
            continue;
        };
        if export_symbol.flags().intersects(SymbolFlags::Alias) {
            export_symbol = checker.get_aliased_symbol(export_symbol);
        }
        // TODO: Possible strada bug - isn't this insufficient in the presence of merge symbols?
        if Some(export_symbol) == d.symbol() {
            return AmbientModuleInfo {
                name: possible_container.name().unwrap().text().to_string(),
                symbol: possible_container.symbol(),
            };
        }
    }
    AmbientModuleInfo::default()
}

// specifiers.go:168
fn get_info(importing_source_file_name: &str, host: &dyn ModuleSpecifierGenerationHost) -> Info {
    let source_directory = tspath::get_directory_path(importing_source_file_name);
    Info {
        importing_source_file_name: importing_source_file_name.to_string(),
        source_directory,
        use_case_sensitive_file_names: host.use_case_sensitive_file_names(),
    }
}

// specifiers.go:180
fn get_all_module_paths(
    info: &Info,
    imported_file_name: &str,
    host: &dyn ModuleSpecifierGenerationHost,
    compiler_options: &CompilerOptions,
    _preferences: &UserPreferences,
    options: ModuleSpecifierOptions,
) -> Vec<ModulePath> {
    // !!! use new cache model
    // importingFilePath := tspath.ToPath(info.ImportingSourceFileName, host.GetCurrentDirectory(), host.UseCaseSensitiveFileNames());
    // importedFilePath := tspath.ToPath(importedFileName, host.GetCurrentDirectory(), host.UseCaseSensitiveFileNames());
    // cache := host.getModuleSpecifierCache();
    // if (cache != nil) {
    //     cached := cache.get(importingFilePath, importedFilePath, preferences, options);
    //     if (cached.modulePaths) {return cached.modulePaths;}
    // }
    // if (cache != nil) {
    //     cache.setModulePaths(importingFilePath, importedFilePath, preferences, options, modulePaths);
    // }
    get_all_module_paths_worker(info, imported_file_name, host, compiler_options, options)
}

// specifiers.go:203
fn get_all_module_paths_worker(
    info: &Info,
    imported_file_name: &str,
    host: &dyn ModuleSpecifierGenerationHost,
    _compiler_options: &CompilerOptions,
    _options: ModuleSpecifierOptions,
) -> Vec<ModulePath> {
    let mut all_file_names: FxHashMap<String, ModulePath> = FxHashMap::default();
    let paths = get_each_file_name_of_module(
        &info.importing_source_file_name,
        imported_file_name,
        host,
        true,
    );
    all_file_names.extend(paths.iter().map(|p| (p.file_name.clone(), p.clone())));

    let use_case_sensitive_file_names = info.use_case_sensitive_file_names;
    let compare_paths = |a: &ModulePath, b: &ModulePath| {
        compare_paths_by_redirect(a, b, use_case_sensitive_file_names)
    };

    // Sort by paths closest to importing file Name directory
    let mut sorted_paths: Vec<ModulePath> = Vec::with_capacity(paths.len());
    let mut directory = info.source_directory.clone();
    while !all_file_names.is_empty() {
        let directory_start = tspath::ensure_trailing_directory_separator(&directory);
        let keys_in_directory: Vec<String> = all_file_names
            .keys()
            .filter(|file_name| file_name.starts_with(&directory_start))
            .cloned()
            .collect();
        let mut paths_in_directory: Vec<ModulePath> = Vec::new();
        paths_in_directory.extend(
            keys_in_directory.iter().map(|file_name| all_file_names.remove(file_name).unwrap()),
        );
        if !paths_in_directory.is_empty() {
            paths_in_directory.sort_by(compare_paths);
            sorted_paths.extend(paths_in_directory);
        }
        let new_directory = tspath::get_directory_path(&directory);
        if new_directory == directory {
            break;
        }
        directory = new_directory;
    }
    if !all_file_names.is_empty() {
        let mut remaining_paths: Vec<ModulePath> = all_file_names.into_values().collect();
        remaining_paths.sort_by(compare_paths);
        sorted_paths.extend(remaining_paths);
    }
    sorted_paths
}

// containsIgnoredPath checks if a path contains patterns that should be ignored.
// This is a local helper that duplicates tspath.ContainsIgnoredPath for performance.
// specifiers.go:252
pub fn contains_ignored_path(s: &str) -> bool {
    s.contains("/node_modules/.") || s.contains("/.git") || s.contains(".#")
}

// ContainsNodeModules checks if a path contains the node_modules directory.
// specifiers.go:259
pub fn contains_node_modules(s: &str) -> bool {
    s.contains("/node_modules/")
}

// GetEachFileNameOfModule returns all possible file paths for a module, including symlink alternatives.
// This function handles symlink resolution and provides multiple path options for module resolution.
// specifiers.go:265
pub fn get_each_file_name_of_module(
    importing_file_name: &str,
    imported_file_name: &str,
    host: &dyn ModuleSpecifierGenerationHost,
    prefer_symlinks: bool,
) -> Vec<ModulePath> {
    let cwd = host.get_current_directory().to_string();
    let imported_path =
        tspath::to_path(imported_file_name, &cwd, host.use_case_sensitive_file_names());
    let mut reference_redirect = String::new();
    let output_and_reference = host.get_project_reference_from_source(&imported_path);
    if let Some(output_and_reference) = output_and_reference
        && !output_and_reference.output_dts.is_empty()
    {
        reference_redirect.clone_from(&output_and_reference.output_dts);
    }

    let redirects = host.get_redirect_targets(&imported_path);
    let mut imported_file_names: Vec<String> = Vec::with_capacity(2 + redirects.len());
    if !reference_redirect.is_empty() {
        imported_file_names.push(reference_redirect.clone());
    }
    imported_file_names.push(imported_file_name.to_string());
    imported_file_names.extend(redirects);
    let targets: Vec<String> =
        imported_file_names.iter().map(|f| tspath::get_normalized_absolute_path(f, &cwd)).collect();
    let mut should_filter_ignored_paths = !targets.iter().all(|t| contains_ignored_path(t));

    let mut results: Vec<ModulePath> = Vec::with_capacity(2);
    if !prefer_symlinks {
        results.extend(targets.iter().filter_map(|p| {
            if should_filter_ignored_paths && contains_ignored_path(p) {
                None
            } else {
                Some(ModulePath {
                    file_name: p.clone(),
                    is_in_node_modules: contains_node_modules(p),
                    is_redirect: reference_redirect == *p,
                })
            }
        }));
    }

    let symlink_cache = host.get_symlink_cache();
    let full_imported_file_name = tspath::get_normalized_absolute_path(imported_file_name, &cwd);
    tspath::for_each_ancestor_directory_stopping_at_global_cache(
        &host.get_global_typings_cache_location(),
        &tspath::get_directory_path(&full_imported_file_name),
        |real_path_directory: &str| -> Option<bool> {
            let symlink_set = symlink_cache.directories_by_realpath().load(
                &tspath::to_path(real_path_directory, &cwd, host.use_case_sensitive_file_names())
                    .ensure_trailing_directory_separator(),
            )?; // Continue to ancestor directory

            // Don't want to a package to globally import from itself (importNameCodeFix_symlink_own_package.ts)
            if tspath::starts_with_directory(
                importing_file_name,
                real_path_directory,
                host.use_case_sensitive_file_names(),
            ) {
                return Some(false); // Stop search, each ancestor directory will also hit this condition
            }

            for target in &targets {
                if !tspath::starts_with_directory(
                    target,
                    real_path_directory,
                    host.use_case_sensitive_file_names(),
                ) {
                    continue;
                }

                let relative = tspath::get_relative_path_from_directory(
                    real_path_directory,
                    target,
                    &tspath::ComparePathsOptions {
                        use_case_sensitive_file_names: host.use_case_sensitive_file_names(),
                        current_directory: cwd.clone(),
                    },
                );
                symlink_set.range(|symlink_directory: &String| {
                    let option = tspath::resolve_path(symlink_directory, &[&relative]);
                    results.push(ModulePath {
                        is_in_node_modules: contains_node_modules(&option),
                        is_redirect: *target == reference_redirect,
                        file_name: option,
                    });
                    should_filter_ignored_paths = true; // We found a non-ignored path in symlinks, so we can reject ignored-path realpaths
                    true
                });
            }

            None
        },
    );

    if prefer_symlinks {
        results.extend(targets.iter().filter_map(|p| {
            if should_filter_ignored_paths && contains_ignored_path(p) {
                None
            } else {
                Some(ModulePath {
                    file_name: p.clone(),
                    is_in_node_modules: contains_node_modules(p),
                    is_redirect: reference_redirect == *p,
                })
            }
        }));
    }

    results
}

// specifiers.go:364
fn compute_module_specifiers(
    module_paths: &[ModulePath],
    compiler_options: &CompilerOptions,
    importing_source_file: P<SourceFile>,
    host: &dyn ModuleSpecifierGenerationHost,
    user_preferences: &UserPreferences,
    options: ModuleSpecifierOptions,
    for_auto_import: bool,
) -> (Vec<String>, ResultKind) {
    let info = get_info(importing_source_file.file_name(), host);
    let preferences = get_module_specifier_preferences(
        user_preferences,
        host,
        compiler_options,
        importing_source_file,
        "",
    );

    let mut existing_specifier = "";
    for module_path in module_paths {
        let target_path = tspath::to_path(
            &module_path.file_name,
            host.get_current_directory(),
            info.use_case_sensitive_file_names,
        );
        let mut existing_import: Option<P<Node>> = None;
        for &import_specifier in importing_source_file.imports() {
            let resolved_module = host
                .get_resolved_module_from_module_specifier(importing_source_file, import_specifier);
            if let Some(resolved_module) = resolved_module.filter(|r| r.is_resolved())
                && tspath::to_path(
                    resolved_module.resolved_file_name,
                    host.get_current_directory(),
                    info.use_case_sensitive_file_names,
                ) == target_path
            {
                existing_import = Some(import_specifier);
                break;
            }
        }
        if let Some(existing_import) = existing_import {
            if preferences.relative_preference == RelativePreferenceKind::NonRelative
                && tspath::path_is_relative(existing_import.text())
            {
                // If the preference is for non-relative and the module specifier is relative, ignore it
                continue;
            }
            let existing_mode =
                host.get_mode_for_usage_location(importing_source_file, existing_import);
            let mut target_mode = options.override_import_mode;
            if target_mode == RESOLUTION_MODE_NONE {
                target_mode = host.get_default_resolution_mode_for_file(importing_source_file);
            }
            if existing_mode != target_mode
                && existing_mode != RESOLUTION_MODE_NONE
                && target_mode != RESOLUTION_MODE_NONE
            {
                // If the candidate import mode doesn't match the mode we're generating for, don't consider it
                continue;
            }
            existing_specifier = existing_import.text();
            break;
        }
    }

    if !existing_specifier.is_empty() {
        return (vec![existing_specifier.to_string()], ResultKind::None);
    }

    let imported_file_is_in_node_modules = module_paths.iter().any(|p| p.is_in_node_modules);

    // Module specifier priority:
    //   1. "Bare package specifiers" (e.g. "@foo/bar") resulting from a path through node_modules to a package.json's "types" entry
    //   2. Specifiers generated using "paths" from tsconfig
    //   3. Non-relative specfiers resulting from a path through node_modules (e.g. "@foo/bar/path/to/file")
    //   4. Relative paths
    let mut paths_specifiers: Vec<String> = Vec::new();
    let mut redirect_paths_specifiers: Vec<String> = Vec::new();
    let mut node_modules_specifiers: Vec<String> = Vec::new();
    let mut relative_specifiers: Vec<String> = Vec::new();

    for module_path in module_paths {
        let mut specifier = String::new();
        if module_path.is_in_node_modules {
            specifier = try_get_module_name_as_node_module(
                module_path,
                &info,
                importing_source_file,
                host,
                compiler_options,
                user_preferences,
                /*packageNameOnly*/ false,
                options.override_import_mode,
            );
        }
        if !(specifier.is_empty()
            || (for_auto_import && is_excluded_by_regex(&specifier, &preferences.exclude_regexes)))
        {
            node_modules_specifiers.push(specifier.clone());
            if module_path.is_redirect {
                // If we got a specifier for a redirect, it was a bare package specifier (e.g. "@foo/bar",
                // not "@foo/bar/path/to/file"). No other specifier will be this good, so stop looking.
                return (node_modules_specifiers, ResultKind::NodeModules);
            }
        }

        let mut import_mode = options.override_import_mode;
        if import_mode == RESOLUTION_MODE_NONE {
            import_mode = host.get_default_resolution_mode_for_file(importing_source_file);
        }
        let local = get_local_module_specifier(
            &module_path.file_name,
            &info,
            compiler_options,
            host,
            import_mode,
            &preferences,
            /*pathsOnly*/ module_path.is_redirect || !specifier.is_empty(),
        );
        if local.is_empty()
            || for_auto_import && is_excluded_by_regex(&local, &preferences.exclude_regexes)
        {
            continue;
        }
        if module_path.is_redirect {
            redirect_paths_specifiers.push(local);
        } else if path_is_bare_specifier(&local) {
            if contains_node_modules(&local) {
                // We could be in this branch due to inappropriate use of `baseUrl`, not intentional `paths`
                // usage. It's impossible to reason about where to prioritize baseUrl-generated module
                // specifiers, but if they contain `/node_modules/`, they're going to trigger a portability
                // error, so *at least* don't prioritize those.
                relative_specifiers.push(local);
            } else {
                paths_specifiers.push(local);
            }
        } else if for_auto_import
            || !imported_file_is_in_node_modules
            || module_path.is_in_node_modules
        {
            // Why this extra conditional, not just an `else`? If some path to the file contained
            // 'node_modules', but we can't create a non-relative specifier (e.g. "@foo/bar/path/to/file"),
            // that means we had to go through a *sibling's* node_modules, not one we can access directly.
            // If some path to the file was in node_modules but another was not, this likely indicates that
            // we have a monorepo structure with symlinks. In this case, the non-nodeModules path is
            // probably the realpath, e.g. "../bar/path/to/file", but a relative path to another package
            // in a monorepo is probably not portable. So, the module specifier we actually go with will be
            // the relative path through node_modules, so that the declaration emitter can produce a
            // portability error. (See declarationEmitReexportedSymlinkReference3)
            relative_specifiers.push(local);
        }
    }

    if !paths_specifiers.is_empty() {
        return (paths_specifiers, ResultKind::Paths);
    }
    if !redirect_paths_specifiers.is_empty() {
        return (redirect_paths_specifiers, ResultKind::Redirect);
    }
    if !node_modules_specifiers.is_empty() {
        return (node_modules_specifiers, ResultKind::NodeModules);
    }
    (relative_specifiers, ResultKind::Relative)
}

// specifiers.go:490
fn get_local_module_specifier(
    module_file_name: &str,
    info: &Info,
    compiler_options: &CompilerOptions,
    host: &dyn ModuleSpecifierGenerationHost,
    import_mode: ModuleKind,
    preferences: &ModuleSpecifierPreferences,
    paths_only: bool,
) -> String {
    let paths = compiler_options.paths.as_ref();
    let root_dirs: &[String] = compiler_options.root_dirs.as_deref().unwrap_or(&[]);

    if paths_only && paths.is_none() {
        return String::new();
    }

    let source_directory = &info.source_directory;

    let allowed_endings =
        preferences.get_allowed_endings_in_preferred_order(host, compiler_options, import_mode);
    let mut relative_path = String::new();
    if !root_dirs.is_empty() {
        relative_path = try_get_module_name_from_root_dirs(
            root_dirs,
            module_file_name,
            source_directory,
            &allowed_endings,
            compiler_options,
            host,
        );
    }
    if relative_path.is_empty() {
        relative_path = process_ending(
            &ensure_path_is_non_module_name(&tspath::get_relative_path_from_directory(
                source_directory,
                module_file_name,
                &tspath::ComparePathsOptions {
                    use_case_sensitive_file_names: host.use_case_sensitive_file_names(),
                    current_directory: host.get_current_directory().to_string(),
                },
            )),
            &allowed_endings,
            compiler_options,
            Some(host),
        );
    }

    if (paths.is_none() && !compiler_options.get_resolve_package_json_imports())
        || preferences.relative_preference == RelativePreferenceKind::Relative
    {
        if paths_only {
            return String::new();
        }
        return relative_path;
    }

    let root = compiler_options.get_paths_base_path(host.get_current_directory());
    let base_directory = tspath::get_normalized_absolute_path(&root, host.get_current_directory());
    let relative_to_base_url = get_relative_path_if_in_same_volume(
        module_file_name,
        &base_directory,
        host.use_case_sensitive_file_names(),
    );
    if relative_to_base_url.is_empty() {
        if paths_only {
            return String::new();
        }
        return relative_path;
    }

    let mut from_package_json_imports = String::new();
    if !paths_only {
        from_package_json_imports = try_get_module_name_from_package_json_imports(
            module_file_name,
            source_directory,
            compiler_options,
            host,
            import_mode,
            prefers_ts_extension(&allowed_endings),
        );
    }

    let mut from_paths = String::new();
    if let Some(paths) = paths.filter(|_| paths_only || from_package_json_imports.is_empty()) {
        from_paths = try_get_module_name_from_paths(
            &relative_to_base_url,
            paths,
            &allowed_endings,
            &base_directory,
            host,
            compiler_options,
        );
    }

    if paths_only {
        return from_paths;
    }

    let maybe_non_relative =
        if from_package_json_imports.is_empty() { from_paths } else { from_package_json_imports };
    if maybe_non_relative.is_empty() {
        return relative_path;
    }

    let relative_is_excluded = is_excluded_by_regex(&relative_path, &preferences.exclude_regexes);
    let non_relative_is_excluded =
        is_excluded_by_regex(&maybe_non_relative, &preferences.exclude_regexes);
    if !relative_is_excluded && non_relative_is_excluded {
        return relative_path;
    }
    if relative_is_excluded && !non_relative_is_excluded {
        return maybe_non_relative;
    }

    if preferences.relative_preference == RelativePreferenceKind::NonRelative
        && !tspath::path_is_relative(&maybe_non_relative)
    {
        return maybe_non_relative;
    }

    if preferences.relative_preference == RelativePreferenceKind::ExternalNonRelative
        && !tspath::path_is_relative(&maybe_non_relative)
    {
        let project_directory = if compiler_options.config_file_path.is_empty() {
            tspath::to_path(
                host.get_current_directory(),
                host.get_current_directory(),
                host.use_case_sensitive_file_names(),
            )
        } else {
            tspath::to_path(
                &tspath::get_directory_path(&compiler_options.config_file_path),
                host.get_current_directory(),
                host.use_case_sensitive_file_names(),
            )
        };
        let canonical_source_directory = tspath::to_path(
            source_directory,
            host.get_current_directory(),
            host.use_case_sensitive_file_names(),
        );
        let module_path = tspath::to_path(
            module_file_name,
            &project_directory,
            host.use_case_sensitive_file_names(),
        );

        let source_is_internal = project_directory.contains_path(&canonical_source_directory);
        let target_is_internal = project_directory.contains_path(&module_path);
        if source_is_internal && !target_is_internal || !source_is_internal && target_is_internal {
            // 1. The import path crosses the boundary of the tsconfig.json-containing directory.
            //
            //      src/
            //        tsconfig.json
            //        index.ts -------
            //      lib/              | (path crosses tsconfig.json)
            //        imported.ts <---
            //
            return maybe_non_relative;
        }

        let nearest_target_package_json = host.get_nearest_ancestor_directory_with_package_json(
            &tspath::get_directory_path(&module_path),
        );
        let nearest_source_package_json =
            host.get_nearest_ancestor_directory_with_package_json(source_directory);

        if !package_json_paths_are_equal(
            &nearest_target_package_json,
            &nearest_source_package_json,
            &tspath::ComparePathsOptions {
                use_case_sensitive_file_names: host.use_case_sensitive_file_names(),
                current_directory: host.get_current_directory().to_string(),
            },
        ) {
            // 2. The importing and imported files are part of different packages.
            //
            //      packages/a/
            //        package.json
            //        index.ts --------
            //      packages/b/        | (path crosses package.json)
            //        package.json     |
            //        component.ts <---
            //
            return maybe_non_relative;
        }

        return relative_path;
    }

    // Prefer a relative import over a baseUrl import if it has fewer components.
    if is_path_relative_to_parent(&maybe_non_relative)
        || count_path_components(&relative_path) < count_path_components(&maybe_non_relative)
    {
        return relative_path;
    }
    maybe_non_relative
}

// specifiers.go:641
fn process_ending(
    file_name: &str,
    allowed_endings: &[ModuleSpecifierEnding],
    options: &CompilerOptions,
    host: Option<&dyn ModuleSpecifierGenerationHost>,
) -> String {
    if tspath::file_extension_is_one_of(
        file_name,
        &[tspath::EXTENSION_JSON, tspath::EXTENSION_MJS, tspath::EXTENSION_CJS],
    ) {
        return file_name.to_string();
    }

    let no_extension = tspath::remove_file_extension(file_name);
    if file_name == no_extension {
        return file_name.to_string();
    }

    let js_priority = index_of_ending(allowed_endings, ModuleSpecifierEnding::JsExtension);
    let ts_priority = index_of_ending(allowed_endings, ModuleSpecifierEnding::TsExtension);
    if tspath::file_extension_is_one_of(file_name, &[tspath::EXTENSION_MTS, tspath::EXTENSION_CTS])
        && matches!((ts_priority, js_priority), (Some(ts), Some(js)) if ts < js)
    {
        return file_name.to_string();
    }
    if tspath::file_extension_is_one_of(
        file_name,
        &[tspath::EXTENSION_DMTS, tspath::EXTENSION_DCTS],
    ) {
        let input_ext = tspath::get_declaration_file_extension(file_name);
        let ext = get_js_extension_for_declaration_file_extension(input_ext);
        return format!("{}{}", tspath::remove_extension(file_name, input_ext), ext);
    }
    if tspath::file_extension_is_one_of(file_name, &[tspath::EXTENSION_MTS, tspath::EXTENSION_CTS])
    {
        return format!("{}{}", no_extension, get_js_extension_for_file(file_name, options));
    }
    if !tspath::file_extension_is_one_of(file_name, &[tspath::EXTENSION_DTS])
        && tspath::file_extension_is_one_of(file_name, &[tspath::EXTENSION_TS])
        && file_name.contains(".d.")
    {
        // `foo.d.json.ts` and the like - remap back to `foo.json`
        let result = try_get_real_file_name_for_non_js_declaration_file_name(file_name);
        if !result.is_empty() {
            return result;
        }
    }

    match allowed_endings[0] {
        ModuleSpecifierEnding::Minimal => {
            let without_index = no_extension.strip_suffix("/index").unwrap_or(no_extension);
            if let Some(host) = host
                && without_index != no_extension
                && try_get_any_file_from_path(host, without_index)
            {
                // Can't remove index if there's a file by the same name as the directory.
                // Probably more callers should pass `host` so we can determine this?
                return no_extension.to_string();
            }
            without_index.to_string()
        }
        ModuleSpecifierEnding::Index => no_extension.to_string(),
        ModuleSpecifierEnding::JsExtension => {
            format!("{}{}", no_extension, get_js_extension_for_file(file_name, options))
        }
        ModuleSpecifierEnding::TsExtension => {
            // For now, we don't know if this import is going to be type-only, which means we don't
            // know if a .d.ts extension is valid, so use no extension or a .js extension
            if tspath::is_declaration_file_name(file_name) {
                let extensionless_priority = allowed_endings.iter().position(|e| {
                    *e == ModuleSpecifierEnding::Minimal || *e == ModuleSpecifierEnding::Index
                });
                if matches!((extensionless_priority, js_priority), (Some(extensionless), Some(js)) if extensionless < js)
                {
                    return no_extension.to_string();
                }
                return format!(
                    "{}{}",
                    no_extension,
                    get_js_extension_for_file(file_name, options)
                );
            }
            file_name.to_string()
        }
    }
}

// specifiers.go:712
fn try_get_module_name_from_root_dirs(
    root_dirs: &[String],
    module_file_name: &str,
    source_directory: &str,
    allowed_endings: &[ModuleSpecifierEnding],
    compiler_options: &CompilerOptions,
    host: &dyn ModuleSpecifierGenerationHost,
) -> String {
    let normalized_target_paths = get_paths_relative_to_root_dirs(
        module_file_name,
        root_dirs,
        host.use_case_sensitive_file_names(),
    );
    if normalized_target_paths.is_empty() {
        return String::new();
    }

    let normalized_source_paths = get_paths_relative_to_root_dirs(
        source_directory,
        root_dirs,
        host.use_case_sensitive_file_names(),
    );
    let mut shortest = String::new();
    let mut shortest_sep_count = 0;
    for source_path in &normalized_source_paths {
        for target_path in &normalized_target_paths {
            let candidate =
                ensure_path_is_non_module_name(&tspath::get_relative_path_from_directory(
                    source_path,
                    target_path,
                    &tspath::ComparePathsOptions {
                        use_case_sensitive_file_names: host.use_case_sensitive_file_names(),
                        current_directory: host.get_current_directory().to_string(),
                    },
                ));
            let candidate_sep_count = candidate.matches('/').count();
            if shortest.is_empty() || candidate_sep_count < shortest_sep_count {
                shortest = candidate;
                shortest_sep_count = candidate_sep_count;
            }
        }
    }

    if shortest.is_empty() {
        return String::new();
    }
    process_ending(&shortest, allowed_endings, compiler_options, Some(host))
}

// specifiers.go:748
fn try_get_module_name_as_node_module(
    path_obj: &ModulePath,
    info: &Info,
    importing_source_file: P<SourceFile>,
    host: &dyn ModuleSpecifierGenerationHost,
    options: &CompilerOptions,
    user_preferences: &UserPreferences,
    package_name_only: bool,
    override_mode: ModuleKind,
) -> String {
    let Some(parts) = get_node_module_path_parts(&path_obj.file_name) else {
        return String::new();
    };

    let case_sensitive = host.use_case_sensitive_file_names();
    let global_typings_cache_location = host.get_global_typings_cache_location();
    // Check reachability before consulting package.json exports, which can return
    // a bare specifier directly. An exported path is only usable if the importing
    // file can reach the package through node_modules, and packages under the
    // global typings cache are excluded.
    let search_root_end =
        parts.top_level_node_modules_index.max(tspath::get_root_length(&path_obj.file_name));
    let search_root = &path_obj.file_name[..search_root_end];
    let compare_options = tspath::ComparePathsOptions {
        use_case_sensitive_file_names: case_sensitive,
        current_directory: host.get_current_directory().to_string(),
    };
    if !tspath::contains_path(search_root, &info.source_directory, &compare_options)
        || !global_typings_cache_location.is_empty()
            && tspath::contains_path(search_root, &global_typings_cache_location, &compare_options)
    {
        return String::new();
    }

    // Simplify the full file path to something that can be resolved by Node.
    let preferences = get_module_specifier_preferences(
        user_preferences,
        host,
        options,
        importing_source_file,
        "",
    );
    let allowed_endings =
        preferences.get_allowed_endings_in_preferred_order(host, options, RESOLUTION_MODE_NONE);

    let mut module_specifier = path_obj.file_name.clone();
    let mut is_package_root_path = false;
    if !package_name_only {
        let mut package_root_index = parts.package_root_index;
        let mut module_file_name = String::new();
        loop {
            let current_parts = NodeModulePathParts { package_root_index, ..parts };
            // If the module could be imported by a directory name, use that directory's name
            let pkg_json_results = try_directory_with_package_json(
                current_parts,
                parts.package_root_index,
                path_obj,
                importing_source_file,
                host,
                override_mode,
                options,
                &allowed_endings,
            );
            let module_file_to_try = pkg_json_results.module_file_to_try;
            let package_root_path = pkg_json_results.package_root_path;
            let blocked_by_exports = pkg_json_results.blocked_by_exports;
            let verbatim_from_exports = pkg_json_results.verbatim_from_exports;
            if blocked_by_exports {
                return String::new(); // File is under this package.json, but is not publicly exported - there's no way to name it via `node_modules` resolution
            }
            if verbatim_from_exports {
                return module_file_to_try;
            }
            //}
            if !package_root_path.is_empty() {
                module_specifier = package_root_path;
                is_package_root_path = true;
                break;
            }
            if module_file_name.is_empty() {
                module_file_name = module_file_to_try;
            }
            // try with next level of directory
            package_root_index = package_root_index
                .and_then(|index| index_after(&path_obj.file_name, "/", index + 1));
            if package_root_index.is_none() {
                module_specifier =
                    process_ending(&module_file_name, &allowed_endings, options, Some(host));
                break;
            }
        }
    }

    if path_obj.is_redirect && !is_package_root_path {
        return String::new();
    }

    // If the module was found in @types, get the actual Node package name
    let node_modules_directory_name = &module_specifier[parts.top_level_package_name_index + 1..];
    tsrs_module::get_package_name_from_types_package_name(node_modules_directory_name)
}

// specifiers.go:837
fn try_directory_with_package_json(
    parts: NodeModulePathParts,
    package_base_root_index: Option<usize>,
    path_obj: &ModulePath,
    importing_source_file: P<SourceFile>,
    host: &dyn ModuleSpecifierGenerationHost,
    override_mode: ModuleKind,
    options: &CompilerOptions,
    allowed_endings: &[ModuleSpecifierEnding],
) -> PkgJsonDirAttemptResult {
    let root_idx = parts.package_root_index.unwrap_or(path_obj.file_name.len());
    let package_root_path = path_obj.file_name[0..root_idx].to_string();
    let package_json_path = tspath::combine_paths(&package_root_path, &["package.json"]);
    let mut module_file_to_try = path_obj.file_name.clone();
    let mut maybe_blocked_by_types_versions = false;
    let Some(package_json) = host.get_package_json_info(&package_json_path) else {
        // No package.json exists; an index.js will still resolve as the package name
        let file_name = &module_file_to_try[package_base_root_index.map_or(0, |index| index + 1)..];
        if file_name == "index.d.ts"
            || file_name == "index.js"
            || file_name == "index.ts"
            || file_name == "index.tsx"
        {
            return PkgJsonDirAttemptResult {
                module_file_to_try,
                package_root_path,
                ..Default::default()
            };
        }
        return PkgJsonDirAttemptResult { module_file_to_try, ..Default::default() };
    };

    let mut import_mode = override_mode;
    if import_mode == RESOLUTION_MODE_NONE {
        import_mode = host.get_default_resolution_mode_for_file(importing_source_file);
    }

    let package_json_content = package_json.get_contents();
    if options.get_resolve_package_json_exports() {
        // The package name that we found in node_modules could be different from the package
        // name in the package.json content via url/filepath dependency specifiers. We need to
        // use the actual directory name, so don't look at `packageJsonContent.name` here.
        let node_modules_directory_name =
            &package_root_path[parts.top_level_package_name_index + 1..];
        let package_name =
            tsrs_module::get_package_name_from_types_package_name(node_modules_directory_name);

        // Determine resolution mode for package.json exports condition matching.
        // TypeScript's tryDirectoryWithPackageJson uses the importing file's mode (moduleSpecifiers.ts:1257),
        // but this causes incorrect exports resolution. We fix this by checking the target file's extension
        // using the logic from getImpliedNodeFormatForEmitWorker (program.ts:4827-4838).
        // .cjs/.cts/.d.cts → CommonJS → "require" condition
        // .mjs/.mts/.d.mts → ESM → "import" condition
        if tspath::file_extension_is_one_of(
            &path_obj.file_name,
            &[tspath::EXTENSION_CJS, tspath::EXTENSION_CTS, tspath::EXTENSION_DCTS],
        ) {
            import_mode = RESOLUTION_MODE_COMMON_JS;
        } else if tspath::file_extension_is_one_of(
            &path_obj.file_name,
            &[tspath::EXTENSION_MJS, tspath::EXTENSION_MTS, tspath::EXTENSION_DMTS],
        ) {
            import_mode = RESOLUTION_MODE_ESM;
        }

        let conditions = tsrs_module::get_conditions(options, import_mode);

        let mut from_exports = String::new();
        if let Some(content) =
            package_json_content.filter(|c| c.fields.exports.type_() != JSONValueType::NotPresent)
        {
            let compute = || {
                try_get_module_name_from_exports(
                    options,
                    host,
                    &path_obj.file_name,
                    &package_root_path,
                    &package_name,
                    &content.fields.exports,
                    &conditions,
                )
            };
            from_exports = match host.exports_module_name_cache(options) {
                Some(cache) => {
                    // the exports map is the one of `package_root_path`'s package.json
                    let mut key =
                        format!("{}\0{}\0{}", path_obj.file_name, package_root_path, package_name);
                    key.extend(conditions.iter().flat_map(|condition| ["\0", condition.as_str()]));
                    cache.get_or_compute(key, compute)
                }
                None => compute(),
            };
        }
        if !from_exports.is_empty() {
            return PkgJsonDirAttemptResult {
                module_file_to_try: from_exports,
                verbatim_from_exports: true,
                ..Default::default()
            };
        }
        if package_json_content
            .is_some_and(|c| c.fields.exports.type_() != JSONValueType::NotPresent)
        {
            return PkgJsonDirAttemptResult {
                module_file_to_try: path_obj.file_name.clone(),
                blocked_by_exports: true,
                ..Default::default()
            };
        }
    }

    let mut version_paths: Option<&packagejson::VersionPaths> = None;
    if let Some(content) =
        package_json_content.filter(|c| c.fields.types_versions.type_() == JSONValueType::Object)
    {
        version_paths = Some(content.get().get_version_paths(None));
    }
    let version_paths_paths = version_paths.and_then(|v| v.get_paths());
    if let Some(paths) = version_paths_paths {
        let sub_module_name = &path_obj.file_name[package_root_path.len() + 1..];
        let from_paths = try_get_module_name_from_paths(
            sub_module_name,
            paths,
            allowed_endings,
            &package_root_path,
            host,
            options,
        );
        if from_paths.is_empty() {
            maybe_blocked_by_types_versions = true;
        } else {
            module_file_to_try = tspath::combine_paths(&package_root_path, &[&from_paths]);
        }
    }
    // If the file is the main module, it can be imported by the package name
    let mut main_file_relative = "index.js".to_string();
    if let Some(content) = package_json_content {
        if content.fields.typings.valid {
            main_file_relative.clone_from(&content.fields.typings.value);
        } else if content.fields.types.valid {
            main_file_relative.clone_from(&content.fields.types.value);
        } else if content.fields.main.valid {
            main_file_relative.clone_from(&content.fields.main.value);
        }
    }

    if !(main_file_relative.is_empty()
        || (maybe_blocked_by_types_versions
            && tsrs_module::match_pattern_or_exact(
                &tsrs_module::try_parse_patterns(version_paths_paths),
                &main_file_relative,
            ) != Pattern::default()))
    {
        // The 'main' file is also subject to mapping through typesVersions, and we couldn't come up with a path
        // explicitly through typesVersions, so if it matches a key in typesVersions now, it's not reachable.
        // (The only way this can happen is if some file in a package that's not resolvable from outside the
        // package got pulled into the program anyway, e.g. transitively through a file that *is* reachable. It
        // happens very easily in fourslash tests though, since every test file listed gets included. See
        // importNameCodeFix_typesVersions.ts for an example.)
        let main_export_file = tspath::to_path(
            &main_file_relative,
            &package_root_path,
            host.use_case_sensitive_file_names(),
        );
        let compare_opt = tspath::ComparePathsOptions {
            use_case_sensitive_file_names: host.use_case_sensitive_file_names(),
            current_directory: host.get_current_directory().to_string(),
        };
        if tspath::compare_paths(
            tspath::remove_file_extension(&main_export_file),
            tspath::remove_file_extension(&module_file_to_try),
            &compare_opt,
        ) == std::cmp::Ordering::Equal
        {
            // ^ An arbitrary removal of file extension for this comparison is almost certainly wrong
            return PkgJsonDirAttemptResult {
                module_file_to_try,
                package_root_path,
                ..Default::default()
            };
        } else if package_json_content.is_none()
            || package_json_content.unwrap().fields.type_.value != "module"
                && !tspath::file_extension_is_one_of(
                    &module_file_to_try,
                    tspath::EXTENSIONS_NOT_SUPPORTING_EXTENSIONLESS_RESOLUTION,
                )
                && stringutil::has_prefix(
                    &module_file_to_try,
                    &main_export_file,
                    host.use_case_sensitive_file_names(),
                )
                && tspath::compare_paths(
                    &tspath::get_directory_path(&module_file_to_try),
                    tspath::remove_trailing_directory_separator(&main_export_file),
                    &compare_opt,
                ) == std::cmp::Ordering::Equal
                && tspath::remove_file_extension(&tspath::get_base_file_name(&module_file_to_try))
                    == "index"
        {
            // if mainExportFile is a directory, which contains moduleFileToTry, we just try index file
            // example mainExportFile: `pkg/lib` and moduleFileToTry: `pkg/lib/index`, we can use packageRootPath
            // but this behavior is deprecated for packages with "type": "module", so we only do this for packages without "type": "module"
            // and make sure that the extension on index.{???} is something that supports omitting the extension
            return PkgJsonDirAttemptResult {
                module_file_to_try,
                package_root_path,
                ..Default::default()
            };
        }
    }

    PkgJsonDirAttemptResult { module_file_to_try, ..Default::default() }
}

// specifiers.go:981
fn try_get_module_name_from_exports(
    options: &CompilerOptions,
    host: &dyn ModuleSpecifierGenerationHost,
    target_file_path: &str,
    package_directory: &str,
    package_name: &str,
    exports: &packagejson::ExportsOrImports,
    conditions: &[String],
) -> String {
    if exports.is_subpaths() {
        // sub-mappings
        // 3 cases:
        // * directory mappings (legacyish, key ends with / (technically allows index/extension resolution under cjs mode))
        // * pattern mappings (contains a *)
        // * exact mappings (no *, does not end with /)
        for (k, subk) in exports.as_object() {
            let sub_package_name = tspath::get_normalized_absolute_path(
                &tspath::combine_paths(package_name, &[k]),
                "",
            );
            let mut mode = MatchingMode::Exact;
            if k.ends_with('/') {
                mode = MatchingMode::Directory;
            } else if k.contains('*') {
                mode = MatchingMode::Pattern;
            }
            let result = try_get_module_name_from_exports_or_imports(
                options,
                host,
                target_file_path,
                package_directory,
                &sub_package_name,
                subk,
                conditions,
                mode,
                /*isImports*/ false,
                /*preferTsExtension*/ false,
            );
            if !result.is_empty() {
                return result;
            }
        }
    }
    try_get_module_name_from_exports_or_imports(
        options,
        host,
        target_file_path,
        package_directory,
        package_name,
        exports,
        conditions,
        MatchingMode::Exact,
        /*isImports*/ false,
        /*preferTsExtension*/ false,
    )
}

// specifiers.go:1024
fn try_get_module_name_from_package_json_imports(
    module_file_name: &str,
    source_directory: &str,
    options: &CompilerOptions,
    host: &dyn ModuleSpecifierGenerationHost,
    import_mode: ModuleKind,
    prefer_ts_extension: bool,
) -> String {
    if !options.get_resolve_package_json_imports() {
        return String::new();
    }

    let ancestor_directory_with_package_json =
        host.get_nearest_ancestor_directory_with_package_json(source_directory);
    if ancestor_directory_with_package_json.is_empty() {
        return String::new();
    }
    let package_json_path =
        tspath::combine_paths(&ancestor_directory_with_package_json, &["package.json"]);

    let Some(info) = host.get_package_json_info(&package_json_path) else {
        return String::new();
    };

    let contents = info.get_contents().unwrap();
    let imports = &contents.fields.imports;
    match imports.type_() {
        JSONValueType::NotPresent | JSONValueType::Array | JSONValueType::String => {
            return String::new(); // not present or invalid for imports
        }
        JSONValueType::Object => {
            let conditions = tsrs_module::get_conditions(options, import_mode);
            let top = imports.as_object();
            for (k, value) in top {
                if k == "#" || k == "#/" || !k.starts_with('#') {
                    continue; // invalid imports entry
                }
                if k.starts_with("#/")
                    && options.get_module_resolution_kind() != ModuleResolutionKind::NODE_NEXT
                    && options.get_module_resolution_kind() != ModuleResolutionKind::BUNDLER
                {
                    continue; // "#/" imports keys are only valid in nodenext/bundler
                }
                let mut mode = MatchingMode::Exact;
                if k.ends_with('/') {
                    mode = MatchingMode::Directory;
                } else if k.contains('*') {
                    mode = MatchingMode::Pattern;
                }
                let result = try_get_module_name_from_exports_or_imports(
                    options,
                    host,
                    module_file_name,
                    &ancestor_directory_with_package_json,
                    k,
                    value,
                    &conditions,
                    mode,
                    true,
                    prefer_ts_extension,
                );
                if !result.is_empty() {
                    return result;
                }
            }
        }
        _ => {}
    }

    String::new()
}

/// Go `strings.Cut(s, sep)`.
fn cut<'a>(s: &'a str, sep: &str) -> (&'a str, &'a str, bool) {
    match s.find(sep) {
        Some(i) => (&s[..i], &s[i + sep.len()..], true),
        None => (s, "", false),
    }
}

// specifiers.go:1094
fn try_get_module_name_from_paths(
    relative_to_base_url: &str,
    paths: &OrderedMap<String, impl PathSubstitutions>,
    allowed_endings: &[ModuleSpecifierEnding],
    base_directory: &str,
    host: &dyn ModuleSpecifierGenerationHost,
    compiler_options: &CompilerOptions,
) -> String {
    let case_sensitive = host.use_case_sensitive_file_names();
    for (key, values) in paths {
        for pattern_text in values.substitutions() {
            let normalized = tspath::normalize_path(pattern_text);
            let mut pattern =
                get_relative_path_if_in_same_volume(&normalized, base_directory, case_sensitive);
            if pattern.is_empty() {
                pattern = normalized;
            }
            let (prefix, suffix, ok) = cut(&pattern, "*");

            // In module resolution, if `pattern` itself has an extension, a file with that extension is looked up directly,
            // meaning a '.ts' or '.d.ts' extension is allowed to resolve. This is distinct from the case where a '*' substitution
            // causes a module specifier to have an extension, i.e. the extension comes from the module specifier in a JS/TS file
            // and matches the '*'. For example:
            //
            // Module Specifier      | Path Mapping (key: [pattern]) | Interpolation       | Resolution Action
            // ---------------------->------------------------------->--------------------->---------------------------------------------------------------
            // import "@app/foo"    -> "@app/*": ["./src/app/*.ts"] -> "./src/app/foo.ts" -> tryFile("./src/app/foo.ts") || [continue resolution algorithm]
            // import "@app/foo.ts" -> "@app/*": ["./src/app/*"]    -> "./src/app/foo.ts" -> [continue resolution algorithm]
            //
            // (https://github.com/microsoft/TypeScript/blob/ad4ded80e1d58f0bf36ac16bea71bc10d9f09895/src/compiler/moduleNameResolver.ts#L2509-L2516)
            //
            // The interpolation produced by both scenarios is identical, but only in the former, where the extension is encoded in
            // the path mapping rather than in the module specifier, will we prioritize a file lookup on the interpolation result.
            // (In fact, currently, the latter scenario will necessarily fail since no resolution mode recognizes '.ts' as a valid
            // extension for a module specifier.)
            //
            // Here, this means we need to be careful about whether we generate a match from the target filename (typically with a
            // .ts extension) or the possible relative module specifiers representing that file:
            //
            // Filename            | Relative Module Specifier Candidates         | Path Mapping                 | Filename Result    | Module Specifier Results
            // --------------------<----------------------------------------------<------------------------------<-------------------||----------------------------
            // dist/haha.d.ts      <- dist/haha, dist/haha.js                     <- "@app/*": ["./dist/*.d.ts"] <- @app/haha        || (none)
            // dist/haha.d.ts      <- dist/haha, dist/haha.js                     <- "@app/*": ["./dist/*"]      <- (none)           || @app/haha, @app/haha.js
            // dist/foo/index.d.ts <- dist/foo, dist/foo/index, dist/foo/index.js <- "@app/*": ["./dist/*.d.ts"] <- @app/foo/index   || (none)
            // dist/foo/index.d.ts <- dist/foo, dist/foo/index, dist/foo/index.js <- "@app/*": ["./dist/*"]      <- (none)           || @app/foo, @app/foo/index, @app/foo/index.js
            // dist/wow.js.js      <- dist/wow.js, dist/wow.js.js                 <- "@app/*": ["./dist/*.js"]   <- @app/wow.js      || @app/wow, @app/wow.js
            //
            // The "Filename Result" can be generated only if `pattern` has an extension. Care must be taken that the list of
            // relative module specifiers to run the interpolation (a) is actually valid for the module resolution mode, (b) takes
            // into account the existence of other files (e.g. 'dist/wow.js' cannot refer to 'dist/wow.js.js' if 'dist/wow.js'
            // exists) and (c) that they are ordered by preference. The last row shows that the filename result and module
            // specifier results are not mutually exclusive. Note that the filename result is a higher priority in module
            // resolution, but as long criteria (b) above is met, I don't think its result needs to be the highest priority result
            // in module specifier generation. I have included it last, as it's difficult to tell exactly where it should be
            // sorted among the others for a particular value of `importModuleSpecifierEnding`.

            let mut candidates: Vec<SpecPair> = Vec::new();
            candidates.extend(allowed_endings.iter().map(|&ending| {
                let result =
                    process_ending(relative_to_base_url, &[ending], compiler_options, Some(host));
                SpecPair { ending, value: result }
            }));
            if !tspath::try_get_extension_from_path(&pattern).is_empty() {
                candidates.push(SpecPair {
                    ending: ModuleSpecifierEnding::JsExtension,
                    value: relative_to_base_url.to_string(),
                });
            }

            if ok {
                if let Some(result) = candidates.iter().find_map(|c| {
                    let value = &c.value;
                    if value.len() >= prefix.len() + suffix.len()
                                    && stringutil::has_prefix(value, prefix, case_sensitive) // TODO: possible strada bug: these are not case-switched in strada
                                    && stringutil::has_suffix(value, suffix, case_sensitive)
                                    && validate_ending(c, relative_to_base_url, compiler_options, host)
                    {
                        let matched_star = &value[prefix.len()..value.len() - suffix.len()];
                        if !tspath::path_is_relative(matched_star) {
                            return Some(replace_first_star(key, matched_star));
                        }
                    }

                    None
                }) {
                    return result;
                }
            } else if candidates
                .iter()
                .any(|c| c.ending != ModuleSpecifierEnding::Minimal && pattern == c.value)
                || candidates.iter().any(|c| {
                    c.ending == ModuleSpecifierEnding::Minimal
                        && pattern == c.value
                        && validate_ending(c, relative_to_base_url, compiler_options, host)
                })
            {
                return key.clone();
            }
        }
    }
    String::new()
}

// specifiers.go:1193
fn validate_ending(
    c: &SpecPair,
    relative_to_base_url: &str,
    compiler_options: &CompilerOptions,
    host: &dyn ModuleSpecifierGenerationHost,
) -> bool {
    // Optimization: `removeExtensionAndIndexPostFix` can query the file system (a good bit) if `ending` is `Minimal`, the basename
    // is 'index', and a `host` is provided. To avoid that until it's unavoidable, we ran the function with no `host` above. Only
    // here, after we've checked that the minimal ending is indeed a match (via the length and prefix/suffix checks / `some` calls),
    // do we check that the host-validated result is consistent with the answer we got before. If it's not, it falls back to the
    // `ModuleSpecifierEnding.Index` result, which should already be in the list of candidates if `Minimal` was. (Note: the assumption here is
    // that every module resolution mode that supports dropping extensions also supports dropping `/index`. Like literally
    // everything else in this file, this logic needs to be updated if that's not true in some future module resolution mode.)
    c.ending != ModuleSpecifierEnding::Minimal
        || c.value
            == process_ending(relative_to_base_url, &[c.ending], compiler_options, Some(host))
}

// specifiers.go:1204
pub fn try_get_module_name_from_exports_or_imports(
    options: &CompilerOptions,
    host: &dyn ModuleSpecifierGenerationHost,
    target_file_path: &str,
    package_directory: &str,
    package_name: &str,
    exports: &packagejson::ExportsOrImports,
    conditions: &[String],
    mode: MatchingMode,
    is_imports: bool,
    prefer_ts_extension: bool,
) -> String {
    match &exports.data {
        JSONData::String(str_value) => {
            // possible strada bug? Always uses compilerOptions of the host project, not those applicable to the targeted package.json!
            let mut output_file = String::new();
            let mut declaration_file = String::new();
            if is_imports {
                let output_paths_host: &dyn outputpaths::OutputPathsHost = host;
                output_file = outputpaths::get_output_js_file_name_worker(
                    target_file_path,
                    options,
                    output_paths_host,
                );
                declaration_file = outputpaths::get_output_declaration_file_name_worker(
                    target_file_path,
                    options,
                    output_paths_host,
                );
            }

            let path_or_pattern = tspath::get_normalized_absolute_path(
                &tspath::combine_paths(package_directory, &[str_value]),
                "",
            );
            let mut extension_swapped_target = String::new();
            if tspath::has_ts_file_extension(target_file_path) {
                extension_swapped_target = format!(
                    "{}{}",
                    tspath::remove_file_extension(target_file_path),
                    tsrs_module::try_get_js_extension_for_file(target_file_path, options)
                );
            }
            let can_try_ts_extension = prefer_ts_extension
                && tspath::has_implementation_ts_file_extension(target_file_path);

            let compare_opts = tspath::ComparePathsOptions {
                use_case_sensitive_file_names: host.use_case_sensitive_file_names(),
                current_directory: host.get_current_directory().to_string(),
            };

            match mode {
                MatchingMode::Exact => {
                    if !extension_swapped_target.is_empty()
                        && tspath::compare_paths(
                            &extension_swapped_target,
                            &path_or_pattern,
                            &compare_opts,
                        ) == std::cmp::Ordering::Equal
                        || tspath::compare_paths(target_file_path, &path_or_pattern, &compare_opts)
                            == std::cmp::Ordering::Equal
                        || !output_file.is_empty()
                            && tspath::compare_paths(&output_file, &path_or_pattern, &compare_opts)
                                == std::cmp::Ordering::Equal
                        || !declaration_file.is_empty()
                            && tspath::compare_paths(
                                &declaration_file,
                                &path_or_pattern,
                                &compare_opts,
                            ) == std::cmp::Ordering::Equal
                    {
                        return package_name.to_string();
                    }
                }
                MatchingMode::Directory => {
                    if can_try_ts_extension
                        && tspath::contains_path(target_file_path, &path_or_pattern, &compare_opts)
                    {
                        let fragment = tspath::get_relative_path_from_directory(
                            &path_or_pattern,
                            target_file_path,
                            &compare_opts,
                        );
                        return tspath::get_normalized_absolute_path(
                            &tspath::combine_paths(
                                &tspath::combine_paths(package_name, &[str_value]),
                                &[&fragment],
                            ),
                            "",
                        );
                    }
                    if !extension_swapped_target.is_empty()
                        && tspath::contains_path(
                            &path_or_pattern,
                            &extension_swapped_target,
                            &compare_opts,
                        )
                    {
                        let fragment = tspath::get_relative_path_from_directory(
                            &path_or_pattern,
                            &extension_swapped_target,
                            &compare_opts,
                        );
                        return tspath::get_normalized_absolute_path(
                            &tspath::combine_paths(
                                &tspath::combine_paths(package_name, &[str_value]),
                                &[&fragment],
                            ),
                            "",
                        );
                    }
                    if !can_try_ts_extension
                        && tspath::contains_path(&path_or_pattern, target_file_path, &compare_opts)
                    {
                        let fragment = tspath::get_relative_path_from_directory(
                            &path_or_pattern,
                            target_file_path,
                            &compare_opts,
                        );
                        return tspath::get_normalized_absolute_path(
                            &tspath::combine_paths(
                                &tspath::combine_paths(package_name, &[str_value]),
                                &[&fragment],
                            ),
                            "",
                        );
                    }
                    if !output_file.is_empty()
                        && tspath::contains_path(&path_or_pattern, &output_file, &compare_opts)
                    {
                        let fragment = tspath::get_relative_path_from_directory(
                            &path_or_pattern,
                            &output_file,
                            &compare_opts,
                        );
                        return tspath::combine_paths(package_name, &[&fragment]);
                    }
                    if !declaration_file.is_empty()
                        && tspath::contains_path(&path_or_pattern, &declaration_file, &compare_opts)
                    {
                        let fragment = tspath::get_relative_path_from_directory(
                            &path_or_pattern,
                            &declaration_file,
                            &compare_opts,
                        );
                        let js_extension = get_js_extension_for_file(&declaration_file, options);
                        let fragment_with_js_extension =
                            tspath::change_extension(&fragment, &js_extension);
                        return tspath::combine_paths(package_name, &[&fragment_with_js_extension]);
                    }
                }
                MatchingMode::Pattern => {
                    let (leading_slice, trailing_slice, _) = cut(&path_or_pattern, "*");
                    let case_sensitive = host.use_case_sensitive_file_names();
                    if can_try_ts_extension
                        && stringutil::has_prefix_and_suffix_without_overlap(
                            target_file_path,
                            leading_slice,
                            trailing_slice,
                            case_sensitive,
                        )
                    {
                        let star_replacement = &target_file_path
                            [leading_slice.len()..target_file_path.len() - trailing_slice.len()];
                        return replace_first_star(package_name, star_replacement);
                    }
                    if !extension_swapped_target.is_empty()
                        && stringutil::has_prefix_and_suffix_without_overlap(
                            &extension_swapped_target,
                            leading_slice,
                            trailing_slice,
                            case_sensitive,
                        )
                    {
                        let star_replacement = &extension_swapped_target[leading_slice.len()
                            ..extension_swapped_target.len() - trailing_slice.len()];
                        return replace_first_star(package_name, star_replacement);
                    }
                    if !can_try_ts_extension
                        && stringutil::has_prefix_and_suffix_without_overlap(
                            target_file_path,
                            leading_slice,
                            trailing_slice,
                            case_sensitive,
                        )
                    {
                        let star_replacement = &target_file_path
                            [leading_slice.len()..target_file_path.len() - trailing_slice.len()];
                        return replace_first_star(package_name, star_replacement);
                    }
                    if !output_file.is_empty()
                        && stringutil::has_prefix_and_suffix_without_overlap(
                            &output_file,
                            leading_slice,
                            trailing_slice,
                            case_sensitive,
                        )
                    {
                        let star_replacement = &output_file
                            [leading_slice.len()..output_file.len() - trailing_slice.len()];
                        return replace_first_star(package_name, star_replacement);
                    }
                    if !declaration_file.is_empty()
                        && stringutil::has_prefix_and_suffix_without_overlap(
                            &declaration_file,
                            leading_slice,
                            trailing_slice,
                            case_sensitive,
                        )
                    {
                        let star_replacement = &declaration_file
                            [leading_slice.len()..declaration_file.len() - trailing_slice.len()];
                        let substituted = replace_first_star(package_name, star_replacement);
                        let js_extension =
                            tsrs_module::try_get_js_extension_for_file(&declaration_file, options);
                        if !js_extension.is_empty() {
                            return tspath::change_full_extension(&substituted, js_extension);
                        }
                    }
                }
            }
            return String::new();
        }
        JSONData::Array(arr) => {
            if let Some(result) = arr.iter().find_map(|e| {
                let result = try_get_module_name_from_exports_or_imports(
                    options,
                    host,
                    target_file_path,
                    package_directory,
                    package_name,
                    e,
                    conditions,
                    mode,
                    is_imports,
                    prefer_ts_extension,
                );
                if !result.is_empty() {
                    return Some(result);
                }

                None
            }) {
                return result;
            }
        }
        JSONData::Object(obj) => {
            // conditional mapping
            if let Some(result) = obj.into_iter().find_map(|(key, value)| {
                if key == "default"
                    || conditions.iter().any(|c| c == key)
                    || conditions.iter().any(|c| c == "types")
                        && tsrs_module::is_applicable_versioned_types_key(key)
                {
                    let result = try_get_module_name_from_exports_or_imports(
                        options,
                        host,
                        target_file_path,
                        package_directory,
                        package_name,
                        value,
                        conditions,
                        mode,
                        is_imports,
                        prefer_ts_extension,
                    );
                    if !result.is_empty() {
                        return Some(result);
                    }
                }

                None
            }) {
                return result;
            }
        }
        JSONData::NotPresent | JSONData::Null => return String::new(),
        _ => {}
    }
    String::new()
}

// `importingSourceFile` and `importingSourceFileName`? Why not just use `importingSourceFile.path`?
// Because when this is called by the declaration emitter, `importingSourceFile` is the implementation
// file, but `importingSourceFileName` and `toFileName` refer to declaration files (the former to the
// one currently being produced; the latter to the one being imported). We need an implementation file
// just to get its `impliedNodeFormat` and to detect certain preferences from existing import module
// specifiers.
// `importing_source_file`: !!! | FutureSourceFile. `old_import_specifier`: used only in updatingModuleSpecifier.

// specifiers.go:1354
pub fn update_module_specifier(
    compiler_options: &CompilerOptions,
    host: &dyn ModuleSpecifierGenerationHost,
    importing_source_file: P<SourceFile>,
    importing_source_file_name: &str,
    old_import_specifier: &str,
    to_file_name: &str,
    user_preferences: &UserPreferences,
    options: ModuleSpecifierOptions,
) -> String {
    get_module_specifier_with_preferences(
        compiler_options,
        host,
        importing_source_file,
        importing_source_file_name,
        old_import_specifier,
        to_file_name,
        user_preferences,
        options,
    )
}

// `importing_source_file`: !!! | FutureSourceFile. `old_import_specifier`: used only in updatingModuleSpecifier.
// specifiers.go:1376
fn get_module_specifier_with_preferences(
    compiler_options: &CompilerOptions,
    host: &dyn ModuleSpecifierGenerationHost,
    importing_source_file: P<SourceFile>,
    importing_source_file_name: &str,
    old_import_specifier: &str,
    to_file_name: &str,
    user_preferences: &UserPreferences,
    options: ModuleSpecifierOptions,
) -> String {
    let info = get_info(importing_source_file_name, host);
    let module_paths = get_all_module_paths(
        &info,
        to_file_name,
        host,
        compiler_options,
        user_preferences,
        options,
    );
    let preferences = get_module_specifier_preferences(
        user_preferences,
        host,
        compiler_options,
        importing_source_file,
        old_import_specifier,
    );

    let mut resolution_mode = options.override_import_mode;
    if resolution_mode == RESOLUTION_MODE_NONE {
        resolution_mode = host.get_default_resolution_mode_for_file(importing_source_file);
    }

    if let Some(result) = module_paths.iter().find_map(|module_path| {
        let first_defined = try_get_module_name_as_node_module(
            module_path,
            &info,
            importing_source_file,
            host,
            compiler_options,
            user_preferences,
            false, /*packageNameOnly*/
            options.override_import_mode,
        );
        if !first_defined.is_empty() {
            return Some(first_defined);
        }

        None
    }) {
        return result;
    }

    get_local_module_specifier(
        to_file_name,
        &info,
        compiler_options,
        host,
        resolution_mode,
        &preferences,
        false,
    )
}
