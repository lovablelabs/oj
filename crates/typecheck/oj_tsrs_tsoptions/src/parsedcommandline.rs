use std::sync::{Mutex, OnceLock};

use rustc_hash::FxHashMap;
use tsrs_ast::Diagnostic;
use tsrs_core::collections::OrderedMap;
use tsrs_core::glob::{self, Glob};
use tsrs_core::tspath::{self, ComparePathsOptions, ExtensionRewrite, Path};
use tsrs_core::{
    CompilerOptions, P, ProjectReference, TypeAcquisition, resolve_project_reference_path,
};
use tsrs_vfs::FS;
use tsrs_vfs::vfsmatch;

use crate::commandlineoption::CompilerOptionsValue;
use crate::contentmappers::Mapper;
use crate::outputpaths::{self, OutputPathsHost};
use crate::parsedoptions::ParsedOptions;
use crate::tsconfigparsing::{TsConfigSourceFile, get_file_names_from_config_specs};
use crate::wildcarddirectories::get_wildcard_directories;

const FILE_GLOB_PATTERN: &str = "*.{js,jsx,mjs,cjs,ts,tsx,mts,cts,json}";
const RECURSIVE_FILE_GLOB_PATTERN: &str = "**/*.{js,jsx,mjs,cjs,ts,tsx,mts,cts,json}";

impl ParsedCommandLine {
    // fileGlobPatterns returns the include file glob patterns for this command line, augmenting the
    // built-in patterns with the extensions registered by its content mappers so that created
    // content-mapped files are recognized as possible root files.
    fn file_glob_patterns(&self) -> (String, String) {
        let mapper_extensions = self.content_mapper_extensions();
        if mapper_extensions.is_empty() {
            return (FILE_GLOB_PATTERN.to_string(), RECURSIVE_FILE_GLOB_PATTERN.to_string());
        }
        let mut extensions: Vec<&str> = Vec::with_capacity(9 + mapper_extensions.len());
        extensions.extend(["js", "jsx", "mjs", "cjs", "ts", "tsx", "mts", "cts", "json"]);
        extensions.extend(
            mapper_extensions
                .iter()
                .map(|extension| extension.strip_prefix('.').unwrap_or(extension)),
        );
        let file_glob = format!("*.{{{}}}", extensions.join(","));
        let recursive = format!("**/{file_glob}");
        (file_glob, recursive)
    }
}

pub struct ParsedCommandLine {
    pub parsed_config: ParsedOptions,

    pub config_file: Option<P<TsConfigSourceFile>>, // TsConfigSourceFile, used in Program and ExecuteCommandLine
    pub errors: Vec<P<Diagnostic>>,
    pub raw: CompilerOptionsValue,
    pub compile_on_save: Option<bool>,

    pub compare_paths_options: ComparePathsOptions,
    pub wildcard_directories: OnceLock<Option<OrderedMap<String, bool>>>,
    pub include_globs: OnceLock<Vec<Glob>>,

    pub source_and_output_maps: OnceLock<SourceAndOutputMaps>,

    pub common_source_directory: OnceLock<String>,
    // Go appends checkSourceFilesBelongToPath diagnostics to Errors from inside CommonSourceDirectory's
    // sync.Once; they are kept here and reported after Errors by GetConfigFileParsingDiagnostics.
    pub common_source_directory_errors: Mutex<Vec<P<Diagnostic>>>,

    pub resolved_project_reference_paths: OnceLock<Vec<String>>,

    pub literal_file_names_len: usize,
    pub file_names_by_path: OnceLock<FxHashMap<Path, String>>, // maps file names to their paths, used for quick lookups
    pub content_mapper_extension_rewrites: OnceLock<Vec<ExtensionRewrite>>,
}

impl ParsedCommandLine {
    pub fn empty() -> ParsedCommandLine {
        ParsedCommandLine {
            parsed_config: ParsedOptions::default(),
            config_file: None,
            errors: Vec::new(),
            raw: CompilerOptionsValue::Null,
            compile_on_save: None,
            compare_paths_options: tsrs_core::tspath::ComparePathsOptions::default(),
            wildcard_directories: OnceLock::new(),
            include_globs: OnceLock::new(),
            source_and_output_maps: OnceLock::new(),
            common_source_directory: OnceLock::new(),
            common_source_directory_errors: Mutex::new(Vec::new()),
            resolved_project_reference_paths: OnceLock::new(),
            literal_file_names_len: 0,
            file_names_by_path: OnceLock::new(),
            content_mapper_extension_rewrites: OnceLock::new(),
        }
    }
}

pub fn new_parsed_command_line(
    compiler_options: P<CompilerOptions>,
    root_file_names: Vec<String>,
    project_references: Vec<ProjectReference>,
    compare_paths_options: ComparePathsOptions,
) -> ParsedCommandLine {
    ParsedCommandLine {
        parsed_config: ParsedOptions {
            compiler_options: Some(compiler_options),
            file_names: root_file_names,
            project_references_is_nil: project_references.is_empty(),
            project_references,
            ..Default::default()
        },
        compare_paths_options,
        ..ParsedCommandLine::empty()
    }
}

fn clone_once_lock<T: Clone>(lock: &OnceLock<T>) -> OnceLock<T> {
    let result = OnceLock::new();
    if let Some(value) = lock.get() {
        let _ = result.set(value.clone());
    }
    result
}

impl ParsedCommandLine {
    #[must_use]
    pub fn with_file_names(&self, file_names: Vec<String>) -> ParsedCommandLine {
        let mut parsed_config = self.parsed_config.clone();
        parsed_config.file_names = file_names;
        ParsedCommandLine {
            parsed_config,
            config_file: self.config_file,
            errors: self.errors.clone(),
            raw: self.raw.clone(),
            compile_on_save: self.compile_on_save,
            compare_paths_options: self.compare_paths_options.clone(),
            wildcard_directories: clone_once_lock(&self.wildcard_directories),
            include_globs: clone_once_lock(&self.include_globs),
            literal_file_names_len: self.literal_file_names_len,
            ..ParsedCommandLine::empty()
        }
    }

    pub fn config_name(&self) -> &str {
        match self.config_file {
            None => "",
            Some(config_file) => config_file.get().source_file.get().file_name(),
        }
    }

    pub fn source_to_project_reference(
        &self,
    ) -> Option<&FxHashMap<Path, P<SourceOutputAndProjectReference>>> {
        self.source_and_output_maps.get().map(|m| &m.source_to_project_reference)
    }

    pub fn output_dts_to_project_reference(
        &self,
    ) -> Option<&FxHashMap<Path, P<SourceOutputAndProjectReference>>> {
        self.source_and_output_maps.get().map(|m| &m.output_dts_to_project_reference)
    }

    // Go method on *ParsedCommandLine; the maps point back at the command line, so it takes the arena pointer.
    pub fn parse_input_output_names(this: P<ParsedCommandLine>) {
        if this.get().source_and_output_maps.get().is_some() {
            return;
        }
        // The maps live as long as the command line, not in the language server's region of whichever program or
        // checker asks first (docs/LSP.md memory regions).
        let _region = tsrs_core::arena::enter_owner(this.addr());
        this.get().source_and_output_maps.get_or_init(|| {
            let p = this.get();
            let mut source_to_output: FxHashMap<Path, P<SourceOutputAndProjectReference>> =
                FxHashMap::default();
            let mut output_dts_to_source: FxHashMap<Path, P<SourceOutputAndProjectReference>> =
                FxHashMap::default();

            for (output_dts, source) in p.get_output_declaration_and_source_file_names() {
                let path = tspath::to_path(
                    &source,
                    p.get_current_directory(),
                    p.use_case_sensitive_file_names(),
                );
                let project_reference = P::new(SourceOutputAndProjectReference {
                    source,
                    output_dts: output_dts.clone(),
                    resolved: this,
                });
                if !output_dts.is_empty() {
                    output_dts_to_source.insert(
                        tspath::to_path(
                            &output_dts,
                            p.get_current_directory(),
                            p.use_case_sensitive_file_names(),
                        ),
                        project_reference,
                    );
                }
                source_to_output.insert(path, project_reference);
            }
            SourceAndOutputMaps {
                source_to_project_reference: source_to_output,
                output_dts_to_project_reference: output_dts_to_source,
            }
        });
    }

    pub fn common_source_directory(&self) -> String {
        self.common_source_directory
            .get_or_init(|| {
                let files = || -> Vec<String> {
                    self.parsed_config
                        .file_names
                        .iter()
                        .filter(|file| {
                            !(tspath::is_declaration_file_name(file)
                                || (self
                                    .compiler_options()
                                    .unwrap()
                                    .no_emit_for_js_files
                                    .is_true()
                                    && tspath::has_js_file_extension(file)))
                        })
                        .cloned()
                        .collect()
                };

                let mut check = |source_files: &[String], root_directory: &str| {
                    self.check_source_files_belong_to_path(source_files, root_directory)
                };
                outputpaths::get_common_source_directory(
                    &self.compiler_options().unwrap(),
                    files,
                    self.get_current_directory(),
                    self.use_case_sensitive_file_names(),
                    Some(&mut check),
                )
            })
            .clone()
    }

    fn check_source_files_belong_to_path(
        &self,
        source_files: &[String],
        root_directory: &str,
    ) -> bool {
        let mut all_files_belong_to_path = true;
        for file in source_files {
            let absolute_source_file_path = tspath::get_canonical_file_name(
                &tspath::get_normalized_absolute_path(file, self.get_current_directory()),
                self.use_case_sensitive_file_names(),
            );
            if !tspath::contains_path(root_directory, file, &self.compare_paths_options) {
                // Kept with the command line, not in the region of whoever asks first (a checker resolving a
                // `rewriteRelativeImportExtensions` import into a referenced project; docs/LSP.md "Memory management").
                let _region = tsrs_core::arena::enter_owner(std::ptr::from_ref(self) as usize);
                self.common_source_directory_errors.lock().unwrap().push(tsrs_ast::new_compiler_diagnostic(
                    &tsrs_diagnostics::FILE_0_IS_NOT_UNDER_ROOT_DIR_1_ROOT_DIR_IS_EXPECTED_TO_CONTAIN_ALL_SOURCE_FILES,
                    &[&absolute_source_file_path, &root_directory],
                ));
                all_files_belong_to_path = false;
            }
        }

        all_files_belong_to_path
    }

    pub fn get_current_directory(&self) -> &str {
        &self.compare_paths_options.current_directory
    }

    pub fn use_case_sensitive_file_names(&self) -> bool {
        self.compare_paths_options.use_case_sensitive_file_names
    }

    fn get_output_declaration_and_source_file_names(&self) -> Vec<(String, String)> {
        let options = self.compiler_options().unwrap();
        let mut result = Vec::with_capacity(self.parsed_config.file_names.len());
        result.extend(self.parsed_config.file_names.iter().map(|file_name| {
            let mut output_dts = String::new();
            if !tspath::is_declaration_file_name(file_name)
                && !tspath::file_extension_is(file_name, tspath::EXTENSION_JSON)
            {
                output_dts =
                    outputpaths::get_output_declaration_file_name_worker(file_name, &options, self);
            }
            (output_dts, file_name.clone())
        }));
        result
    }

    pub fn get_output_file_names(&self) -> Vec<String> {
        let options = self.compiler_options().unwrap();
        let mut result = Vec::new();
        result = self.parsed_config.file_names.iter().fold(result, |mut result, file_name| {
            if tspath::is_declaration_file_name(file_name) {
                return result;
            }
            let js_file_name = outputpaths::get_output_js_file_name(file_name, &options, self);
            let is_json = tspath::file_extension_is(file_name, tspath::EXTENSION_JSON);
            if !js_file_name.is_empty() {
                if is_json {
                    result.push(js_file_name);
                } else {
                    let source_map = outputpaths::get_source_map_file_path(&js_file_name, &options);
                    result.push(js_file_name);
                    if !source_map.is_empty() {
                        result.push(source_map);
                    }
                }
            }
            if is_json {
                return result;
            }
            if options.get_emit_declarations() {
                let dts_file_name =
                    outputpaths::get_output_declaration_file_name_worker(file_name, &options, self);
                if !dts_file_name.is_empty() {
                    let declaration_map = format!("{dts_file_name}.map");
                    result.push(dts_file_name);
                    if self.get_content_mapper_for_file_name(file_name).is_none()
                        && options.get_are_declaration_maps_enabled()
                    {
                        result.push(declaration_map);
                    }
                }
            }

            result
        });
        result
    }

    pub fn get_build_info_file_name(&self) -> String {
        outputpaths::get_build_info_file_name(
            &self.compiler_options().unwrap(),
            &self.compare_paths_options,
        )
    }

    // WildcardDirectories returns the cached wildcard directories, initializing them if needed
    pub fn wildcard_directories(&self) -> Option<&OrderedMap<String, bool>> {
        self.wildcard_directories
            .get_or_init(|| {
                let config_file = self.config_file.unwrap();
                let specs = config_file.config_file_specs.lock().unwrap();
                let specs = specs.as_ref().unwrap();
                get_wildcard_directories(
                    &specs.validated_include_specs,
                    &specs.validated_exclude_specs,
                    &self.compare_paths_options,
                )
            })
            .as_ref()
    }

    fn wildcard_directory_globs(&self) -> &[Glob] {
        let Some(wildcard_directories) = self.wildcard_directories() else {
            return &[];
        };

        self.include_globs.get_or_init(|| {
            let (file_glob, recursive_file_glob) = self.file_glob_patterns();
            let mut globs: Vec<Glob> = Vec::with_capacity(wildcard_directories.len());
            globs.extend(wildcard_directories.iter().filter_map(|(dir, recursive)| {
                let pattern = format!(
                    "{}/{}",
                    tspath::normalize_path(dir),
                    if *recursive { &recursive_file_glob } else { &file_glob }
                );
                glob::parse(&pattern).ok()
            }));
            globs
        })
    }

    // Normalized file names explicitly specified in `files`
    pub fn literal_file_names(&self) -> &[String] {
        if self.config_file.is_some() {
            return &self.file_names()[0..self.literal_file_names_len];
        }
        &[]
    }

    pub fn compiler_options(&self) -> Option<P<CompilerOptions>> {
        self.parsed_config.compiler_options
    }

    pub fn type_acquisition(&self) -> Option<&TypeAcquisition> {
        self.parsed_config.type_acquisition.as_ref()
    }

    // All file names matched by files, include, and exclude patterns
    pub fn file_names(&self) -> &[String] {
        &self.parsed_config.file_names
    }

    pub fn file_names_by_path(&self) -> &FxHashMap<Path, String> {
        self.file_names_by_path.get_or_init(|| {
            let mut file_names_by_path = FxHashMap::with_capacity_and_hasher(
                self.parsed_config.file_names.len(),
                rustc_hash::FxBuildHasher,
            );
            file_names_by_path.extend(self.parsed_config.file_names.iter().map(|file_name| {
                let path = tspath::to_path(
                    file_name,
                    self.get_current_directory(),
                    self.use_case_sensitive_file_names(),
                );
                (path, file_name.clone())
            }));
            file_names_by_path
        })
    }

    pub fn project_references(&self) -> &[ProjectReference] {
        &self.parsed_config.project_references
    }

    // Go `ProjectReferences() == nil`.
    pub fn project_references_is_nil(&self) -> bool {
        self.parsed_config.project_references_is_nil
    }

    pub fn content_mappers(&self) -> &[Mapper] {
        &self.parsed_config.content_mappers
    }

    // ContentMapperExtensions returns the flattened list of file extensions registered by the
    // config's content mappers.
    pub fn content_mapper_extensions(&self) -> Vec<String> {
        self.content_mappers()
            .iter()
            .flat_map(|m| m.definition.extensions.iter().cloned())
            .collect()
    }

    pub fn content_mapper_extension_rewrites(&self) -> &[ExtensionRewrite] {
        self.content_mapper_extension_rewrites
            .get_or_init(|| self.compute_content_mapper_extension_rewrites())
    }

    fn compute_content_mapper_extension_rewrites(&self) -> Vec<ExtensionRewrite> {
        let mut result = Vec::new();
        let mut has_non_identity_mapping = false;
        let ignore_case = !self.use_case_sensitive_file_names();
        for mapper in self.content_mappers() {
            for source in &mapper.definition.extensions {
                let target = get_content_mapper_output_extension(
                    &mapper.definition.output_extensions,
                    source,
                    ignore_case,
                );
                let target = match target {
                    Some(target)
                        if target != source
                            && !(ignore_case
                                && tsrs_core::stringutil::equal_fold(target, source)) =>
                    {
                        has_non_identity_mapping = true;
                        target.clone()
                    }
                    _ => source.clone(),
                };
                result.push(ExtensionRewrite { source: source.clone(), target });
            }
        }
        if !has_non_identity_mapping {
            return Vec::new();
        }
        result.sort_by_key(|rewrite| std::cmp::Reverse(rewrite.source.len()));
        result
    }

    // GetContentMapperForFileName returns the configured content mapper whose extensions include fileName,
    // or nil if no content mapper is registered for the file's extension.
    pub fn get_content_mapper_for_file_name(&self, file_name: &str) -> Option<&Mapper> {
        let ignore_case = !self.use_case_sensitive_file_names();
        let extensions = self.content_mapper_extensions();
        let extension_refs: Vec<&str> =
            extensions.iter().map(std::string::String::as_str).collect();
        let extension =
            tspath::get_longest_extension_from_path(file_name, &extension_refs, ignore_case);
        self.content_mappers().iter().find(|mapper| {
            mapper.definition.extensions.iter().any(|mapper_extension| {
                extension == *mapper_extension
                    || ignore_case
                        && tsrs_core::stringutil::equal_fold(&extension, mapper_extension)
            })
        })
    }

    pub fn resolved_project_reference_paths(&self) -> &[String] {
        self.resolved_project_reference_paths.get_or_init(|| {
            self.parsed_config
                .project_references
                .iter()
                .map(resolve_project_reference_path)
                .collect()
        })
    }

    pub fn extended_source_files(&self) -> Vec<String> {
        match self.config_file {
            None => Vec::new(),
            Some(config_file) => config_file.extended_source_files.lock().unwrap().clone(),
        }
    }

    pub fn get_config_file_parsing_diagnostics(&self) -> Vec<P<Diagnostic>> {
        if let Some(config_file) = self.config_file {
            // todo: !!! should be ConfigFile.ParseDiagnostics, check if they are the same
            let mut result: Vec<P<Diagnostic>> = config_file.source_file.diagnostics().to_vec();
            result.extend(self.errors.iter().copied());
            result.extend(self.common_source_directory_errors.lock().unwrap().iter().copied());
            return result;
        }
        let mut result = self.errors.clone();
        result.extend(self.common_source_directory_errors.lock().unwrap().iter().copied());
        result
    }

    // PossiblyMatchesFileName is a fast check to see if a file is currently included by a config
    // or would be included if the file were to be created. It may return false positives.
    pub fn possibly_matches_file_name(&self, file_name: &str) -> bool {
        let path = tspath::to_path(
            file_name,
            self.get_current_directory(),
            self.use_case_sensitive_file_names(),
        );
        if self.file_names_by_path().contains_key(&path) {
            return true;
        }

        {
            let config_file = self.config_file.unwrap();
            let specs = config_file.config_file_specs.lock().unwrap();
            if specs.as_ref().unwrap().validated_include_specs.iter().any(|include| {
                if !include.contains(['*', '?']) && !vfsmatch::is_implicit_glob(include) {
                    let include_path = tspath::to_path(
                        include,
                        self.get_current_directory(),
                        self.use_case_sensitive_file_names(),
                    );
                    if include_path == path {
                        return true;
                    }
                }

                false
            }) {
                return true;
            }
        }
        if self.get_content_mapper_for_file_name(file_name).is_some() {
            let directory_path = path.get_directory_path();
            if self.possibly_matches_directory_name(&directory_path) {
                return true;
            }
        }
        let wildcard_directory_globs = self.wildcard_directory_globs();
        if !wildcard_directory_globs.is_empty()
            && wildcard_directory_globs.iter().any(|glob| glob.match_(file_name))
        {
            return true;
        }
        false
    }

    pub fn possibly_matches_directory_name(&self, directory_path: &Path) -> bool {
        let Some(wildcard_directories) = self.wildcard_directories() else {
            return false;
        };
        if wildcard_directories.iter().any(|(wildcard_dir, recursive)| {
            let wildcard_dir_path = tspath::to_path(
                wildcard_dir,
                self.get_current_directory(),
                self.use_case_sensitive_file_names(),
            );
            if *recursive {
                if wildcard_dir_path.contains_path(directory_path) {
                    return true;
                }
            } else if wildcard_dir_path == *directory_path {
                return true;
            }

            false
        }) {
            return true;
        }
        false
    }

    pub fn get_matched_file_spec(&self, file_name: &str) -> String {
        let config_file = self.config_file.unwrap();
        let specs = config_file.config_file_specs.lock().unwrap();
        specs.as_ref().unwrap().get_matched_file_spec(file_name, &self.compare_paths_options)
    }

    pub fn get_matched_include_spec(&self, file_name: &str) -> (String, bool) {
        let config_file = self.config_file.unwrap();
        let specs = config_file.config_file_specs.lock().unwrap();
        let specs = specs.as_ref().unwrap();
        if specs.validated_include_specs.is_empty() {
            return (String::new(), false);
        }

        if specs.is_default_include_spec {
            return (specs.validated_include_specs[0].clone(), true);
        }

        (specs.get_matched_include_spec(file_name, &self.compare_paths_options), false)
    }

    #[must_use]
    pub fn reload_file_names_of_parsed_command_line(&self, fs: &dyn FS) -> ParsedCommandLine {
        let mut parsed_config = self.parsed_config.clone();
        let config_file = self.config_file.unwrap();
        let (file_names, literal_file_names_len) = {
            let specs = config_file.config_file_specs.lock().unwrap();
            get_file_names_from_config_specs(
                specs.as_ref().unwrap(),
                self.get_current_directory(),
                self.compiler_options().as_deref(),
                fs,
                &self.content_mapper_extensions(),
            )
        };
        parsed_config.file_names = file_names;
        ParsedCommandLine {
            parsed_config,
            config_file: self.config_file,
            errors: self.errors.clone(),
            raw: self.raw.clone(),
            compile_on_save: self.compile_on_save,
            compare_paths_options: self.compare_paths_options.clone(),
            wildcard_directories: clone_once_lock(&self.wildcard_directories),
            include_globs: clone_once_lock(&self.include_globs),
            literal_file_names_len,
            ..ParsedCommandLine::empty()
        }
    }
}

pub(crate) fn get_content_mapper_output_extension<'a>(
    output_extensions: &'a FxHashMap<String, String>,
    source_extension: &str,
    ignore_case: bool,
) -> Option<&'a String> {
    output_extensions.get(source_extension).or_else(|| {
        ignore_case
            .then(|| {
                output_extensions.iter().find_map(|(source, output)| {
                    tsrs_core::stringutil::equal_fold(source, source_extension).then_some(output)
                })
            })
            .flatten()
    })
}

pub struct SourceOutputAndProjectReference {
    pub source: String,
    pub output_dts: String,
    pub resolved: P<ParsedCommandLine>,
}

pub struct SourceAndOutputMaps {
    source_to_project_reference: FxHashMap<Path, P<SourceOutputAndProjectReference>>,
    output_dts_to_project_reference: FxHashMap<Path, P<SourceOutputAndProjectReference>>,
}

// Go passes `*ParsedCommandLine` where module resolution takes a `module.ResolvedProjectReference`.
impl tsrs_module::ResolvedProjectReference for ParsedCommandLine {
    fn config_name(&self) -> &str {
        ParsedCommandLine::config_name(self)
    }

    fn compiler_options(&self) -> Option<P<CompilerOptions>> {
        ParsedCommandLine::compiler_options(self)
    }
}

impl OutputPathsHost for ParsedCommandLine {
    fn common_source_directory(&self) -> String {
        ParsedCommandLine::common_source_directory(self)
    }

    fn content_mapper_extensions(&self) -> Vec<String> {
        ParsedCommandLine::content_mapper_extensions(self)
    }

    fn content_mapper_extension_rewrites(&self) -> Vec<ExtensionRewrite> {
        ParsedCommandLine::content_mapper_extension_rewrites(self).to_vec()
    }

    fn get_current_directory(&self) -> &str {
        ParsedCommandLine::get_current_directory(self)
    }

    fn use_case_sensitive_file_names(&self) -> bool {
        ParsedCommandLine::use_case_sensitive_file_names(self)
    }
}
