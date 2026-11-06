#![expect(
    clippy::redundant_pub_crate,
    reason = "Only the public API is re-exported; implementation helpers remain crate-private."
)]

use crate::*;
use tsrs_ast::*;
use tsrs_core::tspath;
use tsrs_core::*;

// Non-function declarations in util.go (hand-ported in types.rs):
//   type regexPatternCacheKey (util.go:19)
//   var (util.go:24): regexPatternCacheMu, regexPatternCache
//   type NodeModulePathParts (util.go:263)
//   type nodeModulesPathParseState (util.go:270)
//   const (util.go:272): nodeModulesPathParseStateBeforeNodeModules, nodeModulesPathParseStateNodeModules,
//     nodeModulesPathParseStateScope, nodeModulesPathParseStatePackageContent

// func ProcessEntrypointEnding (util.go:388) is not generated; it is hand-ported below (process_entrypoint_ending).

// util.go:29
pub(crate) fn compare_paths_by_redirect(
    a: &ModulePath,
    b: &ModulePath,
    use_case_sensitive_file_names: bool,
) -> std::cmp::Ordering {
    // Redirects sort first, matching Strada's compareBooleans(b.isRedirect, a.isRedirect).
    let c = compare_booleans(b.is_redirect, a.is_redirect);
    if c != std::cmp::Ordering::Equal {
        return c;
    }
    let c = tspath::compare_number_of_directory_separators(&a.file_name, &b.file_name);
    if c != std::cmp::Ordering::Equal {
        return c;
    }
    // Strada relies on Map insertion order to break remaining ties deterministically;
    // Go maps are unordered, so compare paths to keep the ordering stable.
    tspath::compare_paths(
        &a.file_name,
        &b.file_name,
        &tspath::ComparePathsOptions {
            use_case_sensitive_file_names,
            current_directory: String::new(),
        },
    )
}

// util.go:42
pub(crate) fn path_is_bare_specifier(path: &str) -> bool {
    !tspath::path_is_absolute(path) && !tspath::path_is_relative(path)
}

// util.go:46
pub fn is_excluded_by_regex(module_specifier: &str, excludes: &[String]) -> bool {
    if excludes.iter().any(|pattern| {
        let Some(re) = string_to_regex(pattern) else {
            return false;
        };
        re.is_match(module_specifier)
    }) {
        return true;
    }
    false
}

// util.go:59
fn string_to_regex(pattern: &str) -> Option<P<regex::Regex>> {
    let mut pattern = pattern;
    let mut case_insensitive = false;

    if pattern.len() > 2
        && pattern.as_bytes()[0] == b'/'
        && let Some(last_slash) = pattern.rfind('/').filter(|&index| index > 0)
    {
        let bytes = pattern.as_bytes();
        let has_unescaped_middle_slash =
            (1..last_slash).any(|i| bytes[i] == b'/' && (i == 0 || bytes[i - 1] != b'\\'));

        if !has_unescaped_middle_slash {
            let flags = &pattern[last_slash + 1..];
            pattern = &pattern[1..last_slash];

            case_insensitive |= flags.contains('i');
        }
    }
    let key = RegexPatternCacheKey { pattern: pattern.to_string(), case_insensitive };

    if let Some(re) = REGEX_PATTERN_CACHE.read().unwrap().get(&key) {
        return *re;
    }

    let mut cache = REGEX_PATTERN_CACHE.write().unwrap();

    if let Some(re) = cache.get(&key) {
        return *re;
    }

    if cache.len() > 1000 {
        cache.clear();
    }

    let mut compile_pattern = pattern.to_string();
    if case_insensitive {
        compile_pattern = format!("(?i:{pattern})");
    }

    match regex::Regex::new(&compile_pattern) {
        Err(_) => {
            cache.insert(key, None);
            None
        }
        Ok(compiled) => {
            // The cache is process-wide: never in a freeable region (language server).
            let compiled = {
                let _arena = tsrs_core::arena::enter_thread_arena();
                P::new(compiled)
            };
            cache.insert(key, Some(compiled));
            Some(compiled)
        }
    }
}

/**
 * Ensures a path is either absolute (prefixed with `/` or `c:`) or dot-relative (prefixed
 * with `./` or `../`) so as not to be confused with an unprefixed module name.
 *
 * ```ts
 * ensurePathIsNonModuleName("/path/to/file.ext") === "/path/to/file.ext"
 * ensurePathIsNonModuleName("./path/to/file.ext") === "./path/to/file.ext"
 * ensurePathIsNonModuleName("../path/to/file.ext") === "../path/to/file.ext"
 * ensurePathIsNonModuleName("path/to/file.ext") === "./path/to/file.ext"
 * ```
 *
 */
// util.go:136
pub(crate) fn ensure_path_is_non_module_name(path: &str) -> String {
    if path_is_bare_specifier(path) {
        return format!("./{path}");
    }
    path.to_string()
}

// util.go:143
pub(crate) fn get_js_extension_for_declaration_file_extension(ext: &str) -> String {
    match ext {
        tspath::EXTENSION_DTS => tspath::EXTENSION_JS.to_string(),
        tspath::EXTENSION_DMTS => tspath::EXTENSION_MJS.to_string(),
        tspath::EXTENSION_DCTS => tspath::EXTENSION_CJS.to_string(),
        // .d.json.ts and the like
        _ => ext[".d".len()..ext.len() - tspath::EXTENSION_TS.len()].to_string(),
    }
}

// TryGetRealFileNameForNonJSDeclarationFileName remaps files like `foo.d.json.ts` or
// `foo.module.d.css.ts` back to their real non-JS names.
// util.go:159
pub fn try_get_real_file_name_for_non_js_declaration_file_name(file_name: &str) -> String {
    let base_name = tspath::get_base_file_name(file_name);
    // Ends with .ts, contains ".d.", and is NOT a standard .d.ts file
    if !file_name.ends_with(tspath::EXTENSION_TS)
        || !base_name.contains(".d.")
        || base_name.ends_with(tspath::EXTENSION_DTS)
    {
        return String::new();
    }
    let no_extension = tspath::remove_extension(file_name, tspath::EXTENSION_TS);
    let last_dot_index = no_extension.rfind('.').unwrap();
    let ext = &no_extension[last_dot_index..];
    let before = match no_extension.find(".d.") {
        Some(i) => &no_extension[..i],
        None => no_extension,
    };
    format!("{before}{ext}")
}

// util.go:174
pub(crate) fn get_js_extension_for_file(file_name: &str, options: &CompilerOptions) -> String {
    let result = tsrs_module::try_get_js_extension_for_file(file_name, options);
    assert!(
        !result.is_empty(),
        "Extension {} is unsupported:: FileName:: {}",
        extension_from_path(file_name),
        file_name
    );
    result.to_string()
}

/**
 * Gets the extension from a path.
 * Path must have a valid extension.
 */
// util.go:186
fn extension_from_path(path: &str) -> String {
    let ext = tspath::try_get_extension_from_path(path);
    assert!(!ext.is_empty(), "File {path} has unknown extension.");
    ext.to_string()
}

// util.go:194
pub(crate) fn try_get_any_file_from_path(
    host: &dyn ModuleSpecifierGenerationHost,
    path: &str,
) -> bool {
    // !!! TODO: shouldn't this use readdir instead of fileexists for perf?
    // We check all js, `node` and `json` extensions in addition to TS, since node module resolution would also choose those over the directory
    let ext_groups = tsrs_tsoptions::get_supported_extensions(
        Some(&CompilerOptions { allow_js: Tristate::True, ..Default::default() }),
        &[".node".to_string(), ".json".to_string()],
    );
    if ext_groups.iter().flatten().any(|e| {
        let full_path = format!("{path}{e}");
        host.file_exists(&tspath::get_normalized_absolute_path(
            &full_path,
            host.get_current_directory(),
        ))
    }) {
        return true;
    }
    false
}

// util.go:214
pub(crate) fn get_paths_relative_to_root_dirs(
    path: &str,
    root_dirs: &[String],
    use_case_sensitive_file_names: bool,
) -> Vec<String> {
    let mut results = Vec::new();
    results.extend(root_dirs.iter().filter_map(|root_dir| {
        let relative_path =
            get_relative_path_if_in_same_volume(path, root_dir, use_case_sensitive_file_names);
        if is_path_relative_to_parent(&relative_path) { None } else { Some(relative_path) }
    }));
    results
}

// util.go:225
pub(crate) fn is_path_relative_to_parent(path: &str) -> bool {
    path.starts_with("..")
}

// util.go:229
pub(crate) fn get_relative_path_if_in_same_volume(
    path: &str,
    directory_path: &str,
    use_case_sensitive_file_names: bool,
) -> String {
    let relative_path = tspath::get_relative_path_to_directory_or_url(
        directory_path,
        path,
        false,
        &tspath::ComparePathsOptions {
            use_case_sensitive_file_names,
            current_directory: directory_path.to_string(),
        },
    );
    if tspath::is_rooted_disk_path(&relative_path) {
        return String::new();
    }
    relative_path
}

// util.go:240
pub(crate) fn package_json_paths_are_equal(
    a: &str,
    b: &str,
    options: &tspath::ComparePathsOptions,
) -> bool {
    if a == b {
        return true;
    }
    if a.is_empty() || b.is_empty() {
        return false;
    }
    tspath::compare_paths(a, b, options) == std::cmp::Ordering::Equal
}

// util.go:250
pub(crate) fn prefers_ts_extension(allowed_endings: &[ModuleSpecifierEnding]) -> bool {
    let js_priority = index_of_ending(allowed_endings, ModuleSpecifierEnding::JsExtension);
    let ts_priority = index_of_ending(allowed_endings, ModuleSpecifierEnding::TsExtension);
    matches!((ts_priority, js_priority), (Some(ts), Some(js)) if ts < js)
}

/// Go `slices.Index(allowedEndings, e)`.
pub(crate) fn index_of_ending(
    allowed_endings: &[ModuleSpecifierEnding],
    e: ModuleSpecifierEnding,
) -> Option<usize> {
    allowed_endings.iter().position(|x| *x == e)
}

// util.go:259
pub(crate) fn replace_first_star(s: &str, replacement: &str) -> String {
    s.replacen('*', replacement, 1)
}

// util.go:279
pub fn get_node_module_path_parts(full_path: &str) -> Option<NodeModulePathParts> {
    // If fullPath can't be valid module file within node_modules, returns undefined.
    // Example of expected pattern: /base/path/node_modules/[@scope/otherpackage/@otherscope/node_modules/]package/[subdirectory/]file.js
    // Returns indices:                       ^            ^                                                      ^             ^

    let mut top_level_node_modules_index = 0;
    let mut top_level_package_name_index = 0;
    let mut package_root_index = None;

    let mut part_start = 0;
    let mut part_end = Some(0);
    let mut state = NodeModulesPathParseState::BeforeNodeModules;

    while let Some(end) = part_end {
        part_start = end;
        part_end = index_after(full_path, "/", part_start + 1);
        match state {
            NodeModulesPathParseState::BeforeNodeModules => {
                if full_path[part_start..].starts_with("/node_modules/") {
                    top_level_node_modules_index = part_start;
                    top_level_package_name_index =
                        part_end.expect("node_modules path has a following package segment");
                    state = NodeModulesPathParseState::NodeModules;
                }
            }
            NodeModulesPathParseState::NodeModules | NodeModulesPathParseState::Scope => {
                if state == NodeModulesPathParseState::NodeModules
                    && full_path.as_bytes()[part_start + 1] == b'@'
                {
                    state = NodeModulesPathParseState::Scope;
                } else {
                    package_root_index = part_end;
                    state = NodeModulesPathParseState::PackageContent;
                }
            }
            NodeModulesPathParseState::PackageContent => {
                if full_path[part_start..].starts_with("/node_modules/") {
                    state = NodeModulesPathParseState::NodeModules;
                } else {
                    state = NodeModulesPathParseState::PackageContent;
                }
            }
        }
    }

    let file_name_index = part_start;

    if state > NodeModulesPathParseState::NodeModules {
        return Some(NodeModulePathParts {
            top_level_node_modules_index,
            top_level_package_name_index,
            package_root_index,
            file_name_index,
        });
    }
    None
}

// util.go:359
pub fn get_package_name_from_directory(file_or_directory_path: &str) -> String {
    let Some(i) = file_or_directory_path.rfind("/node_modules/") else {
        return String::new();
    };
    let basename = &file_or_directory_path[i + "/node_modules/".len()..];

    if basename.as_bytes()[0] == b'.' {
        return String::new();
    }

    let Some(next_slash) = basename.find('/') else {
        return basename.to_string();
    };

    if basename.as_bytes()[0] != b'@' || next_slash == basename.len() - 1 {
        return basename[..next_slash].to_string();
    }

    let Some(second_slash) = basename[next_slash + 1..].find('/') else {
        return basename.to_string();
    };

    basename[..next_slash + 1 + second_slash].to_string()
}

// util.go:388
// ProcessEntrypointEnding processes a pre-computed module specifier from a package.json exports
// entrypoint according to the entrypoint's Ending type and the user's preferred endings.
pub fn process_entrypoint_ending(
    entrypoint: &tsrs_module::ResolvedEntrypoint,
    prefs: &UserPreferences,
    host: &dyn ModuleSpecifierGenerationHost,
    options: &CompilerOptions,
    importing_source_file: P<SourceFile>,
    allowed_endings: &[ModuleSpecifierEnding],
) -> String {
    let mut specifier = entrypoint.module_specifier.clone();
    if entrypoint.ending == tsrs_module::Ending::Fixed {
        return specifier;
    }

    let computed;
    let mut allowed_endings = allowed_endings;
    if allowed_endings.is_empty() {
        computed = get_allowed_endings_in_preferred_order(
            prefs,
            host,
            options,
            importing_source_file,
            "",
            host.get_default_resolution_mode_for_file(importing_source_file),
        );
        allowed_endings = &computed;
    }

    let preferred_ending = allowed_endings[0];

    // Handle declaration file extensions
    let dts_extension = tspath::get_declaration_file_extension(&specifier);
    if !dts_extension.is_empty() {
        match preferred_ending {
            ModuleSpecifierEnding::TsExtension | ModuleSpecifierEnding::JsExtension => {
                // Map .d.ts -> .js, .d.mts -> .mjs, .d.cts -> .cjs
                let js_extension = get_js_extension_for_declaration_file_extension(dts_extension);
                return tspath::change_any_extension(
                    &specifier,
                    &js_extension,
                    &[dts_extension],
                    false,
                );
            }
            ModuleSpecifierEnding::Minimal | ModuleSpecifierEnding::Index => {
                if entrypoint.ending == tsrs_module::Ending::Changeable {
                    // .d.mts/.d.cts must keep an extension; rewrite to .mjs/.cjs instead of dropping
                    if dts_extension == tspath::EXTENSION_DTS {
                        specifier = tspath::remove_extension(&specifier, dts_extension).to_string();
                        if preferred_ending == ModuleSpecifierEnding::Minimal
                            && let Some(s) = specifier.strip_suffix("/index")
                        {
                            specifier = s.to_string();
                        }
                        return specifier;
                    }
                    let js_extension =
                        get_js_extension_for_declaration_file_extension(dts_extension);
                    return tspath::change_any_extension(
                        &specifier,
                        &js_extension,
                        &[dts_extension],
                        false,
                    );
                }
                // EndingExtensionChangeable - can only change extension, not remove it
                let js_extension = get_js_extension_for_declaration_file_extension(dts_extension);
                return tspath::change_any_extension(
                    &specifier,
                    &js_extension,
                    &[dts_extension],
                    false,
                );
            }
        }
    }

    // Handle .ts/.tsx/.mts/.cts extensions
    if tspath::file_extension_is_one_of(
        &specifier,
        &[
            tspath::EXTENSION_TS,
            tspath::EXTENSION_TSX,
            tspath::EXTENSION_MTS,
            tspath::EXTENSION_CTS,
        ],
    ) {
        match preferred_ending {
            ModuleSpecifierEnding::TsExtension => return specifier,
            ModuleSpecifierEnding::JsExtension => {
                let js_extension = tsrs_module::try_get_js_extension_for_file(&specifier, options);
                if !js_extension.is_empty() {
                    return format!(
                        "{}{}",
                        tspath::remove_file_extension(&specifier),
                        js_extension
                    );
                }
                return specifier;
            }
            ModuleSpecifierEnding::Minimal | ModuleSpecifierEnding::Index => {
                if entrypoint.ending == tsrs_module::Ending::Changeable {
                    specifier = tspath::remove_file_extension(&specifier).to_string();
                    if preferred_ending == ModuleSpecifierEnding::Minimal
                        && let Some(s) = specifier.strip_suffix("/index")
                    {
                        specifier = s.to_string();
                    }
                    return specifier;
                }
                // EndingExtensionChangeable - can only change extension, not remove it
                let js_extension = tsrs_module::try_get_js_extension_for_file(&specifier, options);
                if !js_extension.is_empty() {
                    return format!(
                        "{}{}",
                        tspath::remove_file_extension(&specifier),
                        js_extension
                    );
                }
                return specifier;
            }
        }
    }

    // Handle .js/.jsx/.mjs/.cjs extensions
    if tspath::file_extension_is_one_of(
        &specifier,
        &[
            tspath::EXTENSION_JS,
            tspath::EXTENSION_JSX,
            tspath::EXTENSION_MJS,
            tspath::EXTENSION_CJS,
        ],
    ) {
        match preferred_ending {
            ModuleSpecifierEnding::TsExtension | ModuleSpecifierEnding::JsExtension => {
                return specifier;
            }
            ModuleSpecifierEnding::Minimal | ModuleSpecifierEnding::Index => {
                if entrypoint.ending == tsrs_module::Ending::Changeable {
                    specifier = tspath::remove_file_extension(&specifier).to_string();
                    if preferred_ending == ModuleSpecifierEnding::Minimal
                        && let Some(s) = specifier.strip_suffix("/index")
                    {
                        specifier = s.to_string();
                    }
                    return specifier;
                }
                // EndingExtensionChangeable - keep the extension
                return specifier;
            }
        }
    }

    // For other extensions (like .json), return as-is
    specifier
}
