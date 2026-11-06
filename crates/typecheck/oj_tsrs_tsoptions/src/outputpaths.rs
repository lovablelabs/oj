// Go package `outputpaths`.

use tsrs_ast::{self as ast, SourceFile};
use tsrs_core::tspath::{self, ComparePathsOptions, ExtensionRewrite};
use tsrs_core::{CompilerOptions, JsxEmit, P};

// commonsourcedirectory.go

fn compute_common_source_directory_of_filenames(
    file_names: &[String],
    current_directory: &str,
    use_case_sensitive_file_names: bool,
) -> String {
    let mut common_path_components: Option<Vec<String>> = None;
    for source_file in file_names {
        // Each file contributes into common source file path
        let mut source_path_components =
            tspath::get_normalized_path_components(source_file, current_directory);

        // The base file name is not part of the common directory path
        source_path_components.pop();

        let Some(common) = &mut common_path_components else {
            // first file
            common_path_components = Some(source_path_components);
            continue;
        };

        let n = common.len().min(source_path_components.len());
        for i in 0..n {
            if tspath::get_canonical_file_name(&common[i], use_case_sensitive_file_names)
                != tspath::get_canonical_file_name(
                    &source_path_components[i],
                    use_case_sensitive_file_names,
                )
            {
                if i == 0 {
                    // Failed to find any common path component
                    return String::new();
                }

                // New common path found that is 0 -> i-1
                common.truncate(i);
                break;
            }
        }

        // If the sourcePathComponents was shorter than the commonPathComponents, truncate to the sourcePathComponents
        if source_path_components.len() < common.len() {
            common.truncate(source_path_components.len());
        }
    }

    match common_path_components {
        Some(common) if !common.is_empty() => tspath::get_path_from_path_components(&common),
        // Can happen when all input files are .d.ts files
        _ => current_directory.to_string(),
    }
}

pub fn get_computed_common_source_directory(
    emitted_files: &[String],
    current_directory: &str,
    use_case_sensitive_file_names: bool,
) -> String {
    let mut common_source_directory = compute_common_source_directory_of_filenames(
        emitted_files,
        current_directory,
        use_case_sensitive_file_names,
    );
    if !common_source_directory.is_empty() {
        common_source_directory =
            tspath::ensure_trailing_directory_separator(&common_source_directory);
    }
    common_source_directory
}

pub fn get_common_source_directory(
    options: &CompilerOptions,
    files: impl FnOnce() -> Vec<String>,
    current_directory: &str,
    use_case_sensitive_file_names: bool,
    check_source_files_belong_to_path: Option<&mut dyn FnMut(&[String], &str) -> bool>,
) -> String {
    let mut common_source_directory;
    if !options.root_dir.is_empty() {
        // If a rootDir is specified use it as the commonSourceDirectory
        common_source_directory = options.root_dir.clone();
        if let Some(check) = check_source_files_belong_to_path {
            check(&files(), &options.root_dir);
        }
    } else if !options.config_file_path.is_empty() {
        // If the rootDir is not specified, then the common source directory is the directory of the config file.
        common_source_directory = tspath::get_directory_path(&options.config_file_path);
        if let Some(check) = check_source_files_belong_to_path {
            check(&files(), &common_source_directory);
        }
    } else {
        common_source_directory = compute_common_source_directory_of_filenames(
            &files(),
            current_directory,
            use_case_sensitive_file_names,
        );
    }

    if !common_source_directory.is_empty() {
        // Make sure directory path ends with directory separator so this string can directly
        // used to replace with "" to get the relative path of the source file and the relative path doesn't
        // start with / making it rooted path
        common_source_directory =
            tspath::ensure_trailing_directory_separator(&common_source_directory);
    }

    common_source_directory
}

// outputpaths.go

pub trait OutputPathsHost {
    fn common_source_directory(&self) -> String;
    fn content_mapper_extensions(&self) -> Vec<String>;
    fn content_mapper_extension_rewrites(&self) -> Vec<ExtensionRewrite> {
        Vec::new()
    }
    fn get_current_directory(&self) -> &str;
    fn use_case_sensitive_file_names(&self) -> bool;
}

#[derive(Default)]
pub struct OutputPaths {
    js_file_path: String,
    source_map_file_path: String,
    declaration_file_path: String,
    declaration_map_path: String,
    external_output_file_path: String,
}

impl OutputPaths {
    pub fn declaration_file_path(&self) -> &str {
        &self.declaration_file_path
    }

    pub fn js_file_path(&self) -> &str {
        &self.js_file_path
    }

    pub fn source_map_file_path(&self) -> &str {
        &self.source_map_file_path
    }

    pub fn declaration_map_path(&self) -> &str {
        &self.declaration_map_path
    }

    pub fn external_output_file_path(&self) -> &str {
        &self.external_output_file_path
    }
}

#[derive(Default, Clone, Copy)]
pub struct ForceEmitPaths {
    pub dts: bool,
    pub js: bool,
    pub declaration_map: bool,
}

pub fn get_output_paths_for(
    source_file: P<SourceFile>,
    options: &CompilerOptions,
    host: &dyn OutputPathsHost,
    force: ForceEmitPaths,
) -> OutputPaths {
    let own_output_file_path = get_own_emit_output_file_path(
        source_file.file_name(),
        options,
        host,
        get_output_extension(source_file.file_name(), options.jsx),
    );
    let is_json_file = ast::is_json_source_file(source_file);
    // If json file emits to the same location skip writing it, if emitDeclarationOnly skip writing it
    let is_json_emitted_to_same_location = is_json_file
        && tspath::compare_paths(
            source_file.file_name(),
            &own_output_file_path,
            &ComparePathsOptions {
                current_directory: host.get_current_directory().to_string(),
                use_case_sensitive_file_names: host.use_case_sensitive_file_names(),
            },
        ) == std::cmp::Ordering::Equal;
    let mut paths = OutputPaths::default();
    if !source_file.content_mapper().is_empty() {
        paths.external_output_file_path =
            get_external_output_file_name(source_file.file_name(), options, host);
    }
    if source_file.content_mapper().is_empty()
        && (force.js || options.emit_declaration_only != tsrs_core::Tristate::True)
        && !is_json_emitted_to_same_location
    {
        paths.js_file_path = own_output_file_path;
        if !ast::is_json_source_file(source_file) {
            paths.source_map_file_path = get_source_map_file_path(&paths.js_file_path, options);
        }
    }
    if force.dts || options.get_emit_declarations() && !is_json_file {
        paths.declaration_file_path =
            get_declaration_emit_output_file_path(source_file.file_name(), options, host);
        if options.get_are_declaration_maps_enabled()
            || force.declaration_map && options.declaration_map.is_true()
        {
            paths.declaration_map_path = format!("{}.map", paths.declaration_file_path);
        }
    }
    paths
}

pub fn for_each_emitted_file(
    host: &dyn OutputPathsHost,
    options: &CompilerOptions,
    mut action: impl FnMut(&OutputPaths, P<SourceFile>) -> bool,
    source_files: &[P<SourceFile>],
    external_output_source_files: &[P<SourceFile>],
    force_dts_emit: bool,
) -> bool {
    let mut seen = rustc_hash::FxHashSet::default();
    for &source_file in source_files {
        seen.insert(source_file);
        if action(
            &get_output_paths_for(
                source_file,
                options,
                host,
                ForceEmitPaths { dts: force_dts_emit, ..Default::default() },
            ),
            source_file,
        ) {
            return true;
        }
    }
    for &source_file in external_output_source_files {
        if seen.contains(&source_file) {
            continue;
        }
        let paths = OutputPaths {
            external_output_file_path: get_external_output_file_name(
                source_file.file_name(),
                options,
                host,
            ),
            ..OutputPaths::default()
        };
        if action(&paths, source_file) {
            return true;
        }
    }
    false
}

fn extension_refs(extensions: &[String]) -> Vec<&str> {
    extensions.iter().map(std::string::String::as_str).collect()
}

pub(crate) fn get_output_js_file_name(
    input_file_name: &str,
    options: &CompilerOptions,
    host: &dyn OutputPathsHost,
) -> String {
    if options.emit_declaration_only.is_true() || is_content_mapped_file_name(input_file_name, host)
    {
        return String::new();
    }
    let output_file_name = get_output_js_file_name_worker(input_file_name, options, host);
    if !tspath::file_extension_is(&output_file_name, tspath::EXTENSION_JSON)
        || tspath::compare_paths(
            input_file_name,
            &output_file_name,
            &ComparePathsOptions {
                current_directory: host.get_current_directory().to_string(),
                use_case_sensitive_file_names: host.use_case_sensitive_file_names(),
            },
        ) != std::cmp::Ordering::Equal
    {
        return output_file_name;
    }

    String::new()
}

fn is_content_mapped_file_name(file_name: &str, host: &dyn OutputPathsHost) -> bool {
    let extensions = host.content_mapper_extensions();
    !tspath::get_longest_extension_from_path(
        file_name,
        &extension_refs(&extensions),
        !host.use_case_sensitive_file_names(),
    )
    .is_empty()
}

pub fn get_output_js_file_name_worker(
    input_file_name: &str,
    options: &CompilerOptions,
    host: &dyn OutputPathsHost,
) -> String {
    tspath::change_extension(
        &get_output_path_without_changing_extension(input_file_name, &options.out_dir, host),
        get_output_extension(input_file_name, options.jsx),
    )
}

pub fn get_output_declaration_file_name_worker(
    input_file_name: &str,
    options: &CompilerOptions,
    host: &dyn OutputPathsHost,
) -> String {
    let mut dir = options.declaration_dir.as_str();
    if dir.is_empty() {
        dir = &options.out_dir;
    }
    change_to_declaration_extension(
        &get_output_path_without_changing_extension(input_file_name, dir, host),
        host,
    )
}

pub fn get_external_output_file_name(
    input_file_name: &str,
    options: &CompilerOptions,
    host: &dyn OutputPathsHost,
) -> String {
    let output_path =
        get_output_path_without_changing_extension(input_file_name, &options.out_dir, host);
    let (rewritten, did_rewrite) = tspath::rewrite_extension(
        &output_path,
        &host.content_mapper_extension_rewrites(),
        host.use_case_sensitive_file_names(),
    );
    if did_rewrite { rewritten } else { String::new() }
}

pub fn get_output_extension(file_name: &str, jsx: JsxEmit) -> &'static str {
    if tspath::file_extension_is(file_name, tspath::EXTENSION_JSON) {
        tspath::EXTENSION_JSON
    } else if jsx == JsxEmit::PRESERVE
        && tspath::file_extension_is_one_of(
            file_name,
            &[tspath::EXTENSION_JSX, tspath::EXTENSION_TSX],
        )
    {
        tspath::EXTENSION_JSX
    } else if tspath::file_extension_is_one_of(
        file_name,
        &[tspath::EXTENSION_MTS, tspath::EXTENSION_MJS],
    ) {
        tspath::EXTENSION_MJS
    } else if tspath::file_extension_is_one_of(
        file_name,
        &[tspath::EXTENSION_CTS, tspath::EXTENSION_CJS],
    ) {
        tspath::EXTENSION_CJS
    } else {
        tspath::EXTENSION_JS
    }
}

fn get_declaration_emit_output_file_path(
    file: &str,
    options: &CompilerOptions,
    host: &dyn OutputPathsHost,
) -> String {
    let output_dir = if !options.declaration_dir.is_empty() {
        Some(&options.declaration_dir)
    } else if !options.out_dir.is_empty() {
        Some(&options.out_dir)
    } else {
        None
    };

    let path = match output_dir {
        Some(output_dir) => get_source_file_path_in_new_dir_worker(
            file,
            output_dir,
            host.get_current_directory(),
            &host.common_source_directory(),
            host.use_case_sensitive_file_names(),
        ),
        None => file.to_string(),
    };
    change_to_declaration_extension(&path, host)
}

pub fn change_to_declaration_extension(path: &str, host: &dyn OutputPathsHost) -> String {
    let (rewritten, did_rewrite) = tspath::rewrite_extension(
        path,
        &host.content_mapper_extension_rewrites(),
        host.use_case_sensitive_file_names(),
    );
    if did_rewrite {
        return change_to_declaration_extension_worker(&rewritten);
    }
    let extensions = host.content_mapper_extensions();
    let extension =
        tspath::get_longest_extension_from_path(path, &extension_refs(&extensions), false);
    if !extension.is_empty() {
        return format!("{}.d{}.ts", tspath::remove_extension(path, &extension), extension);
    }
    change_to_declaration_extension_worker(path)
}

fn change_to_declaration_extension_worker(path: &str) -> String {
    let mut path_without_extension = tspath::remove_file_extension(path);
    if path_without_extension == path {
        let extension = tspath::get_any_extension_from_path(path, &[], false);
        if !extension.is_empty() {
            path_without_extension = tspath::remove_extension(path, &extension);
        }
    }
    format!("{}{}", path_without_extension, tspath::get_declaration_emit_extension_for_path(path))
}

pub fn get_source_file_path_in_new_dir(
    file_name: &str,
    new_dir_path: &str,
    current_directory: &str,
    common_source_directory: &str,
    use_case_sensitive_file_names: bool,
) -> String {
    get_source_file_path_in_new_dir_worker(
        file_name,
        new_dir_path,
        current_directory,
        common_source_directory,
        use_case_sensitive_file_names,
    )
}

fn get_output_path_without_changing_extension(
    input_file_name: &str,
    output_directory: &str,
    host: &dyn OutputPathsHost,
) -> String {
    if !output_directory.is_empty() {
        return tspath::resolve_path(
            output_directory,
            &[&tspath::get_relative_path_from_directory(
                &host.common_source_directory(),
                input_file_name,
                &ComparePathsOptions {
                    use_case_sensitive_file_names: host.use_case_sensitive_file_names(),
                    current_directory: host.get_current_directory().to_string(),
                },
            )],
        );
    }
    input_file_name.to_string()
}

pub fn get_source_file_path_in_new_dir_worker(
    file_name: &str,
    new_dir_path: &str,
    current_directory: &str,
    common_source_directory: &str,
    use_case_sensitive_file_names: bool,
) -> String {
    let mut source_file_path = tspath::get_normalized_absolute_path(file_name, current_directory);
    if let Some(trimmed) = tspath::trim_file_path_prefix(
        &source_file_path,
        common_source_directory,
        use_case_sensitive_file_names,
    ) {
        source_file_path = trimmed.to_string();
    }
    tspath::combine_paths(new_dir_path, &[&source_file_path])
}

fn get_own_emit_output_file_path(
    file_name: &str,
    options: &CompilerOptions,
    host: &dyn OutputPathsHost,
    extension: &str,
) -> String {
    let emit_output_file_path_without_extension = if options.out_dir.is_empty() {
        tspath::remove_file_extension(file_name).to_string()
    } else {
        let current_directory = host.get_current_directory();
        tspath::remove_file_extension(&get_source_file_path_in_new_dir(
            file_name,
            &options.out_dir,
            current_directory,
            &host.common_source_directory(),
            host.use_case_sensitive_file_names(),
        ))
        .to_string()
    };
    emit_output_file_path_without_extension + extension
}

pub(crate) fn get_source_map_file_path(js_file_path: &str, options: &CompilerOptions) -> String {
    if options.source_map.is_true() && !options.inline_source_map.is_true() {
        return format!("{js_file_path}.map");
    }
    String::new()
}

pub fn get_build_info_file_name(options: &CompilerOptions, opts: &ComparePathsOptions) -> String {
    if !options.is_incremental() && !options.build.is_true() {
        return String::new();
    }
    if !options.ts_build_info_file.is_empty() {
        return options.ts_build_info_file.clone();
    }
    if options.config_file_path.is_empty() {
        return String::new();
    }
    let config_file_extension_less = tspath::remove_file_extension(&options.config_file_path);
    let build_info_extension_less = if options.out_dir.is_empty() {
        config_file_extension_less.to_string()
    } else if options.root_dir.is_empty() {
        tspath::combine_paths(
            &options.out_dir,
            &[&tspath::get_base_file_name(config_file_extension_less)],
        )
    } else {
        tspath::resolve_path(
            &options.out_dir,
            &[&tspath::get_relative_path_from_directory(
                &options.root_dir,
                config_file_extension_less,
                opts,
            )],
        )
    };
    build_info_extension_less + tspath::EXTENSION_TS_BUILD_INFO
}
