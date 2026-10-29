use rustc_hash::FxHashSet;
use tsrs_ast::Diagnostic;
use tsrs_core::collections::OrderedMap;
use tsrs_core::tspath;
use tsrs_core::{
    BuildOptions, CompilerOptions, JsxEmit, ModuleDetectionKind, ModuleKind, ModuleResolutionKind,
    NewLineKind, P, PluginImport, ProjectReference, ScriptTarget, Tristate, TypeAcquisition,
};
use tsrs_diagnostics as diagnostics;
use tsrs_diagnostics::Message;

use crate::commandlineoption::{CommandLineOption, CommandLineOptionKind, CompilerOptionsValue};
use crate::contentmappers::Mapper;
use crate::enummaps::{
    JSX_OPTION_MAP, MODULE_DETECTION_OPTION_MAP, MODULE_OPTION_MAP, MODULE_RESOLUTION_OPTION_MAP,
    NEW_LINE_OPTION_MAP, TARGET_OPTION_MAP,
};
use crate::errors::{extra_key_diagnostics, extra_key_did_you_mean_diagnostics};
use crate::namemap::BUILD_NAME_MAP;
use crate::tsconfigparsing::{COMMAND_LINE_COMPILER_OPTIONS_MAP, CommandLineOptionNameMap};
use tsrs_ast::new_compiler_diagnostic;

fn parse_tristate(value: &CompilerOptionsValue) -> Tristate {
    match value {
        CompilerOptionsValue::Null => Tristate::Unknown,
        CompilerOptionsValue::Tristate(v) => *v,
        CompilerOptionsValue::Bool(true) => Tristate::True,
        _ => Tristate::False,
    }
}

fn parse_string_array(value: &CompilerOptionsValue) -> Option<Vec<String>> {
    if let CompilerOptionsValue::Array(arr) = value {
        let mut result = Vec::with_capacity(arr.len());
        result.extend(arr.iter().filter_map(|v| {
            if let CompilerOptionsValue::String(str) = v { Some(str.clone()) } else { None }
        }));
        return Some(result);
    }
    None
}

fn parse_string_map(
    value: &CompilerOptionsValue,
) -> Option<OrderedMap<String, Option<Vec<String>>>> {
    if let CompilerOptionsValue::Object(m) = value {
        let mut result = OrderedMap::with_capacity_and_hasher(m.len(), rustc_hash::FxBuildHasher);
        result.extend(m.iter().map(|(k, v)| (k.clone(), parse_string_array(v))));
        return Some(result);
    }
    None
}

fn parse_string(value: &CompilerOptionsValue) -> String {
    if let CompilerOptionsValue::String(str) = value {
        return str.clone();
    }
    String::new()
}

// parseNumber into a Go `*int` (64-bit) option.
fn parse_number_int(value: &CompilerOptionsValue) -> Option<i64> {
    match value {
        CompilerOptionsValue::Int(num) => Some(*num),
        CompilerOptionsValue::Float(num) => Some(*num as i64),
        _ => None,
    }
}

pub(crate) struct ProjectReferenceParseResult {
    pub(crate) reference: ProjectReference,
    pub(crate) has_path: bool,
    pub(crate) path_valid: bool,
    pub(crate) has_circular: bool,
    pub(crate) circular_valid: bool,
}

pub(crate) fn parse_project_reference(
    json: &CompilerOptionsValue,
) -> Option<ProjectReferenceParseResult> {
    if let CompilerOptionsValue::Object(v) = json {
        let mut result = ProjectReferenceParseResult {
            reference: ProjectReference::default(),
            has_path: false,
            path_valid: false,
            has_circular: false,
            circular_valid: false,
        };
        if let Some(value) = v.get("path") {
            result.has_path = true;
            if let CompilerOptionsValue::String(path) = value {
                result.reference.path.clone_from(path);
                result.path_valid = true;
            }
        }
        if let Some(value) = v.get("circular") {
            result.has_circular = true;
            if let CompilerOptionsValue::Bool(circular) = value {
                result.reference.circular = *circular;
                result.circular_valid = true;
            }
        }
        return Some(result);
    }
    None
}

pub(crate) fn parse_content_mapper(
    value: &CompilerOptionsValue,
) -> (Option<Mapper>, Vec<P<Diagnostic>>) {
    let CompilerOptionsValue::Object(v) = value else {
        return (None, Vec::new());
    };
    let mut errors = Vec::new();
    let mut mapper = Mapper::default();
    if let Some(pkg) = v.get("package") {
        match pkg {
            CompilerOptionsValue::String(str) if !str.is_empty() => {
                mapper.definition.package.clone_from(str);
            }
            _ => errors.push(new_compiler_diagnostic(
                &diagnostics::COMPILER_OPTION_0_REQUIRES_A_VALUE_OF_TYPE_1,
                &[&"contentMapper.package", &"string"],
            )),
        }
    } else {
        errors.push(new_compiler_diagnostic(
            &diagnostics::COMPILER_OPTION_0_REQUIRES_A_VALUE_OF_TYPE_1,
            &[&"contentMapper.package", &"string"],
        ));
    }
    if let Some(extensions) = v.get("extensions") {
        if let Some(strs) = parse_string_array_strict(extensions) {
            mapper.definition.extensions = strs;
        } else {
            errors.push(new_compiler_diagnostic(
                &diagnostics::COMPILER_OPTION_0_REQUIRES_A_VALUE_OF_TYPE_1,
                &[&"contentMapper.extensions", &"string[]"],
            ));
        }
    } else {
        errors.push(new_compiler_diagnostic(
            &diagnostics::COMPILER_OPTION_0_REQUIRES_A_VALUE_OF_TYPE_1,
            &[&"contentMapper.extensions", &"string[]"],
        ));
    }
    if let Some(output_extensions) = v.get("outputExtensions")
        && !matches!(output_extensions, CompilerOptionsValue::Null)
    {
        if let Some(values) = parse_string_string_map_strict(output_extensions) {
            mapper.definition.output_extensions = values;
            mapper.definition.output_extensions_present = true;
        } else {
            errors.push(new_compiler_diagnostic(
                &diagnostics::COMPILER_OPTION_0_REQUIRES_A_VALUE_OF_TYPE_1,
                &[&"contentMapper.outputExtensions", &"object or null"],
            ));
        }
    }
    if let Some(options) = v.get("options") {
        if matches!(options, CompilerOptionsValue::Object(_)) {
            mapper.definition.options = crate::tsconfigparsing::stringify_json(options);
        } else {
            errors.push(new_compiler_diagnostic(
                &diagnostics::COMPILER_OPTION_0_REQUIRES_A_VALUE_OF_TYPE_1,
                &[&"contentMapper.options", &"object"],
            ));
        }
    }
    if !errors.is_empty() {
        return (None, errors);
    }
    (Some(mapper), errors)
}

fn parse_string_string_map_strict(
    value: &CompilerOptionsValue,
) -> Option<rustc_hash::FxHashMap<String, String>> {
    let CompilerOptionsValue::Object(map) = value else {
        return None;
    };
    let mut result = rustc_hash::FxHashMap::default();
    for (key, value) in map {
        let CompilerOptionsValue::String(value) = value else {
            return None;
        };
        result.insert(key.clone(), value.clone());
    }
    Some(result)
}

// parseStringArrayStrict returns the string slice and true only if value is an array whose
// elements are all strings. A missing element or wrong element type yields false.
fn parse_string_array_strict(value: &CompilerOptionsValue) -> Option<Vec<String>> {
    let CompilerOptionsValue::Array(arr) = value else {
        return None;
    };
    let mut result = Vec::with_capacity(arr.len());
    result = arr.iter().try_fold(result, |mut result, v| {
        let CompilerOptionsValue::String(str) = v else {
            return None;
        };
        result.push(str.clone());

        Some(result)
    })?;
    Some(result)
}

pub(crate) fn parse_json_to_string_key(
    json: &CompilerOptionsValue,
) -> OrderedMap<String, CompilerOptionsValue> {
    let mut result = OrderedMap::with_capacity_and_hasher(6, rustc_hash::FxBuildHasher);
    if let CompilerOptionsValue::Object(m) = json {
        if let Some(v) = m.get("include") {
            result.insert("include".to_string(), v.clone());
        }
        if let Some(v) = m.get("exclude") {
            result.insert("exclude".to_string(), v.clone());
        }
        if let Some(v) = m.get("files") {
            result.insert("files".to_string(), v.clone());
        }
        if let Some(v) = m.get("references") {
            result.insert("references".to_string(), v.clone());
        }
        if let Some(v) = m.get("contentMappers") {
            result.insert("contentMappers".to_string(), v.clone());
        }
        if let Some(v) = m.get("extends") {
            if let CompilerOptionsValue::String(str) = v {
                result.insert(
                    "extends".to_string(),
                    CompilerOptionsValue::Array(vec![CompilerOptionsValue::String(str.clone())]),
                );
            }
            result.insert("extends".to_string(), v.clone());
        }
        if let Some(v) = m.get("compilerOptions") {
            result.insert("compilerOptions".to_string(), v.clone());
        }
        if let Some(v) = m.get("excludes") {
            result.insert("excludes".to_string(), v.clone());
        }
        if let Some(v) = m.get("typeAcquisition") {
            result.insert("typeAcquisition".to_string(), v.clone());
        }
    }
    result
}

pub trait OptionParser {
    fn parse_option(&mut self, key: &str, value: &CompilerOptionsValue) -> Vec<P<Diagnostic>>;
    fn unknown_option_diagnostic(&self) -> &'static Message;
    fn unknown_did_you_mean_diagnostic(&self) -> &'static Message;
}

pub struct CompilerOptionsParser(pub(crate) CompilerOptions);

impl OptionParser for CompilerOptionsParser {
    fn parse_option(&mut self, key: &str, value: &CompilerOptionsValue) -> Vec<P<Diagnostic>> {
        parse_compiler_options(key, value, &mut self.0)
    }

    fn unknown_option_diagnostic(&self) -> &'static Message {
        extra_key_diagnostics("compilerOptions").unwrap()
    }

    fn unknown_did_you_mean_diagnostic(&self) -> &'static Message {
        extra_key_did_you_mean_diagnostics("compilerOptions").unwrap()
    }
}

pub struct TypeAcquisitionParser(pub(crate) TypeAcquisition);

impl OptionParser for TypeAcquisitionParser {
    fn parse_option(&mut self, key: &str, value: &CompilerOptionsValue) -> Vec<P<Diagnostic>> {
        parse_type_acquisition(key, value, &mut self.0)
    }

    fn unknown_option_diagnostic(&self) -> &'static Message {
        extra_key_diagnostics("typeAcquisition").unwrap()
    }

    fn unknown_did_you_mean_diagnostic(&self) -> &'static Message {
        extra_key_did_you_mean_diagnostics("typeAcquisition").unwrap()
    }
}

pub struct BuildOptionsParser(pub(crate) BuildOptions);

impl OptionParser for BuildOptionsParser {
    fn parse_option(&mut self, key: &str, value: &CompilerOptionsValue) -> Vec<P<Diagnostic>> {
        parse_build_options(key, value, &mut self.0)
    }

    fn unknown_option_diagnostic(&self) -> &'static Message {
        extra_key_diagnostics("buildOptions").unwrap()
    }

    fn unknown_did_you_mean_diagnostic(&self) -> &'static Message {
        extra_key_did_you_mean_diagnostics("buildOptions").unwrap()
    }
}

// Go `ParseCompilerOptions`.
pub fn parse_compiler_options(
    key: &str,
    value: &CompilerOptionsValue,
    all_options: &mut CompilerOptions,
) -> Vec<P<Diagnostic>> {
    if value.is_null() {
        return Vec::new();
    }
    parse_compiler_options_worker(key, value, all_options);
    Vec::new()
}

// Go `parseCompilerOptions`.
pub fn parse_compiler_options_worker(
    key: &str,
    value: &CompilerOptionsValue,
    all_options: &mut CompilerOptions,
) -> bool {
    let option = COMMAND_LINE_COMPILER_OPTIONS_MAP.get(key);
    let key = match option {
        Some(option) => option.name,
        None => key,
    };
    match key {
        "allowJs" => all_options.allow_js = parse_tristate(value),
        "allowImportingTsExtensions" => {
            all_options.allow_importing_ts_extensions = parse_tristate(value);
        }
        "allowSyntheticDefaultImports" => {
            all_options.allow_synthetic_default_imports = parse_tristate(value);
        }
        "allowNonTsExtensions" => all_options.allow_non_ts_extensions = parse_tristate(value),
        "allowUmdGlobalAccess" => all_options.allow_umd_global_access = parse_tristate(value),
        "allowUnreachableCode" => all_options.allow_unreachable_code = parse_tristate(value),
        "allowUnusedLabels" => all_options.allow_unused_labels = parse_tristate(value),
        "allowArbitraryExtensions" => {
            all_options.allow_arbitrary_extensions = parse_tristate(value);
        }
        "alwaysStrict" => all_options.always_strict = parse_tristate(value),
        "assumeChangesOnlyAffectDirectDependencies" => {
            all_options.assume_changes_only_affect_direct_dependencies = parse_tristate(value);
        }
        "baseUrl" => all_options.base_url = parse_string(value),
        "build" => all_options.build = parse_tristate(value),
        "checkJs" => all_options.check_js = parse_tristate(value),
        "customConditions" => all_options.custom_conditions = parse_string_array(value),
        "composite" => all_options.composite = parse_tristate(value),
        "declarationDir" => all_options.declaration_dir = parse_string(value),
        "deduplicatePackages" => all_options.deduplicate_packages = parse_tristate(value),
        "diagnostics" => all_options.diagnostics = parse_tristate(value),
        "disableSizeLimit" => all_options.disable_size_limit = parse_tristate(value),
        "disableSourceOfProjectReferenceRedirect" => {
            all_options.disable_source_of_project_reference_redirect = parse_tristate(value);
        }
        "disableSolutionSearching" => {
            all_options.disable_solution_searching = parse_tristate(value);
        }
        "disableReferencedProjectLoad" => {
            all_options.disable_referenced_project_load = parse_tristate(value);
        }
        "declarationMap" => all_options.declaration_map = parse_tristate(value),
        "declaration" => all_options.declaration = parse_tristate(value),
        "downlevelIteration" => all_options.downlevel_iteration = parse_tristate(value),
        "erasableSyntaxOnly" => all_options.erasable_syntax_only = parse_tristate(value),
        "emitDeclarationOnly" => all_options.emit_declaration_only = parse_tristate(value),
        "extendedDiagnostics" => all_options.extended_diagnostics = parse_tristate(value),
        "emitDecoratorMetadata" => all_options.emit_decorator_metadata = parse_tristate(value),
        "emitBOM" => all_options.emit_bom = parse_tristate(value),
        "esModuleInterop" => all_options.es_module_interop = parse_tristate(value),
        "exactOptionalPropertyTypes" => {
            all_options.exact_optional_property_types = parse_tristate(value);
        }
        "explainFiles" => all_options.explain_files = parse_tristate(value),
        "experimentalDecorators" => all_options.experimental_decorators = parse_tristate(value),
        "forceConsistentCasingInFileNames" => {
            all_options.force_consistent_casing_in_file_names = parse_tristate(value);
        }
        "generateCpuProfile" => all_options.generate_cpu_profile = parse_string(value),
        "generateTrace" => all_options.generate_trace = parse_string(value),
        "isolatedModules" => all_options.isolated_modules = parse_tristate(value),
        "ignoreConfig" => all_options.ignore_config = parse_tristate(value),
        "ignoreDeprecations" => all_options.ignore_deprecations = parse_string(value),
        "importHelpers" => all_options.import_helpers = parse_tristate(value),
        "incremental" => all_options.incremental = parse_tristate(value),
        "init" => all_options.init = parse_tristate(value),
        "inlineSourceMap" => all_options.inline_source_map = parse_tristate(value),
        "inlineSources" => all_options.inline_sources = parse_tristate(value),
        "isolatedDeclarations" => all_options.isolated_declarations = parse_tristate(value),
        "jsx" => all_options.jsx = float_or_int32_to_jsx_emit(value),
        "jsxFactory" => all_options.jsx_factory = parse_string(value),
        "jsxFragmentFactory" => all_options.jsx_fragment_factory = parse_string(value),
        "jsxImportSource" => all_options.jsx_import_source = parse_string(value),
        "lib" => {
            if let CompilerOptionsValue::StringArray(v) = value {
                all_options.lib = Some(v.clone());
            } else {
                all_options.lib = parse_string_array(value);
            }
        }
        "libReplacement" => all_options.lib_replacement = parse_tristate(value),
        "listEmittedFiles" => all_options.list_emitted_files = parse_tristate(value),
        "listFiles" => all_options.list_files = parse_tristate(value),
        "listFilesOnly" => all_options.list_files_only = parse_tristate(value),
        "locale" => all_options.locale = parse_string(value),
        "mapRoot" => all_options.map_root = parse_string(value),
        "module" => all_options.module = float_or_int32_to_module_kind(value),
        "moduleDetectionKind" | "moduleDetection" => {
            all_options.module_detection = float_or_int32_to_module_detection_kind(value);
        }
        "moduleResolution" => {
            all_options.module_resolution = float_or_int32_to_module_resolution_kind(value);
        }
        "moduleSuffixes" => all_options.module_suffixes = parse_string_array(value),
        "noCheck" => all_options.no_check = parse_tristate(value),
        "noFallthroughCasesInSwitch" => {
            all_options.no_fallthrough_cases_in_switch = parse_tristate(value);
        }
        "noEmitForJsFiles" => all_options.no_emit_for_js_files = parse_tristate(value),
        "noErrorTruncation" => all_options.no_error_truncation = parse_tristate(value),
        "noImplicitAny" => all_options.no_implicit_any = parse_tristate(value),
        "noImplicitThis" => all_options.no_implicit_this = parse_tristate(value),
        "noLib" => all_options.no_lib = parse_tristate(value),
        "noPropertyAccessFromIndexSignature" => {
            all_options.no_property_access_from_index_signature = parse_tristate(value);
        }
        "noUncheckedIndexedAccess" => {
            all_options.no_unchecked_indexed_access = parse_tristate(value);
        }
        "noEmitHelpers" => all_options.no_emit_helpers = parse_tristate(value),
        "noEmitOnError" => all_options.no_emit_on_error = parse_tristate(value),
        "noImplicitReturns" => all_options.no_implicit_returns = parse_tristate(value),
        "noUnusedLocals" => all_options.no_unused_locals = parse_tristate(value),
        "noUnusedParameters" => all_options.no_unused_parameters = parse_tristate(value),
        "noImplicitOverride" => all_options.no_implicit_override = parse_tristate(value),
        "noUncheckedSideEffectImports" => {
            all_options.no_unchecked_side_effect_imports = parse_tristate(value);
        }
        "outFile" => all_options.out_file = parse_string(value),
        "noResolve" => all_options.no_resolve = parse_tristate(value),
        "paths" => all_options.paths = parse_string_map(value).map(std::sync::Arc::new),
        "plugins" => {
            // Native TypeScript does not load plugins; retain them only so tools can report the incompatibility.
            if matches!(value, CompilerOptionsValue::NilArray) {
                all_options.plugins = None;
            }
            if let CompilerOptionsValue::Array(plugins) = value {
                all_options.plugins = Some(
                    plugins
                        .iter()
                        .map(|plugin| {
                            if let CompilerOptionsValue::Object(plugin_map) = plugin {
                                return PluginImport {
                                    name: parse_string(
                                        plugin_map
                                            .get("name")
                                            .unwrap_or(&CompilerOptionsValue::Null),
                                    ),
                                };
                            }
                            PluginImport::default()
                        })
                        .collect(),
                );
            }
        }
        "preserveWatchOutput" => all_options.preserve_watch_output = parse_tristate(value),
        "preserveConstEnums" => all_options.preserve_const_enums = parse_tristate(value),
        "preserveSymlinks" => all_options.preserve_symlinks = parse_tristate(value),
        "project" => all_options.project = parse_string(value),
        "pretty" => all_options.pretty = parse_tristate(value),
        "resolveJsonModule" => all_options.resolve_json_module = parse_tristate(value),
        "resolvePackageJsonExports" => {
            all_options.resolve_package_json_exports = parse_tristate(value);
        }
        "resolvePackageJsonImports" => {
            all_options.resolve_package_json_imports = parse_tristate(value);
        }
        "reactNamespace" => all_options.react_namespace = parse_string(value),
        "rewriteRelativeImportExtensions" => {
            all_options.rewrite_relative_import_extensions = parse_tristate(value);
        }
        "rootDir" => all_options.root_dir = parse_string(value),
        "rootDirs" => all_options.root_dirs = parse_string_array(value),
        "removeComments" => all_options.remove_comments = parse_tristate(value),
        "stableTypeOrdering" => all_options.stable_type_ordering = parse_tristate(value),
        "strict" => all_options.strict = parse_tristate(value),
        "strictBindCallApply" => all_options.strict_bind_call_apply = parse_tristate(value),
        "strictBuiltinIteratorReturn" => {
            all_options.strict_builtin_iterator_return = parse_tristate(value);
        }
        "strictFunctionTypes" => all_options.strict_function_types = parse_tristate(value),
        "strictNullChecks" => all_options.strict_null_checks = parse_tristate(value),
        "strictPropertyInitialization" => {
            all_options.strict_property_initialization = parse_tristate(value);
        }
        "skipDefaultLibCheck" => all_options.skip_default_lib_check = parse_tristate(value),
        "sourceMap" => all_options.source_map = parse_tristate(value),
        "sourceRoot" => all_options.source_root = parse_string(value),
        "stripInternal" => all_options.strip_internal = parse_tristate(value),
        "suppressOutputPathCheck" => all_options.suppress_output_path_check = parse_tristate(value),
        "target" => all_options.target = float_or_int32_to_script_target(value),
        "traceResolution" => all_options.trace_resolution = parse_tristate(value),
        "tsBuildInfoFile" => all_options.ts_build_info_file = parse_string(value),
        "typeRoots" => all_options.type_roots = parse_string_array(value),
        "types" => all_options.types = parse_string_array(value),
        "useDefineForClassFields" => {
            all_options.use_define_for_class_fields = parse_tristate(value);
        }
        "useUnknownInCatchVariables" => {
            all_options.use_unknown_in_catch_variables = parse_tristate(value);
        }
        "verbatimModuleSyntax" => all_options.verbatim_module_syntax = parse_tristate(value),
        "version" => all_options.version = parse_tristate(value),
        "help" => all_options.help = parse_tristate(value),
        "all" => all_options.all = parse_tristate(value),
        "maxNodeModuleJsDepth" => all_options.max_node_module_js_depth = parse_number_int(value),
        "skipLibCheck" => all_options.skip_lib_check = parse_tristate(value),
        "noEmit" => all_options.no_emit = parse_tristate(value),
        "showConfig" => all_options.show_config = parse_tristate(value),
        "configFilePath" => all_options.config_file_path = parse_string(value),
        "noDtsResolution" => all_options.no_dts_resolution = parse_tristate(value),
        "pathsBasePath" => all_options.paths_base_path = parse_string(value),
        "outDir" => all_options.out_dir = parse_string(value),
        "newLine" => all_options.new_line = float_or_int32_to_new_line_kind(value),
        "watch" => all_options.watch = parse_tristate(value),
        "pprofDir" => all_options.pprof_dir = parse_string(value),
        "singleThreaded" => all_options.single_threaded = parse_tristate(value),
        "quiet" => all_options.quiet = parse_tristate(value),
        "checkers" => all_options.checkers = parse_number_int(value),
        "runExternalCode" => all_options.run_external_code = parse_tristate(value),
        _ => {
            // different than any key above
            return false;
        }
    }
    true
}

// floatOrInt32ToFlag[T]: the value is either already the enum (from the enum maps) or a float64
// (options that came from JSON). Every enum value a float can name appears in the option's enum map.
macro_rules! float_or_int32_to_flag {
    ($fn_name:ident, $ty:ident, $map:ident) => {
        fn $fn_name(value: &CompilerOptionsValue) -> $ty {
            match value {
                CompilerOptionsValue::$ty(v) => *v,
                CompilerOptionsValue::Float(f) => {
                    let n = *f as i32;
                    if let Some(value) = $map.values().find_map(|v| {
                        if let CompilerOptionsValue::$ty(v) = v {
                            if v.value() == n {
                                return Some(*v);
                            }
                        }
                        None
                    }) {
                        return value;
                    }
                    $ty::default()
                }
                _ => panic!("interface conversion: value is not {}", stringify!($ty)),
            }
        }
    };
}

float_or_int32_to_flag!(float_or_int32_to_jsx_emit, JsxEmit, JSX_OPTION_MAP);
float_or_int32_to_flag!(float_or_int32_to_module_kind, ModuleKind, MODULE_OPTION_MAP);
float_or_int32_to_flag!(
    float_or_int32_to_module_detection_kind,
    ModuleDetectionKind,
    MODULE_DETECTION_OPTION_MAP
);
float_or_int32_to_flag!(
    float_or_int32_to_module_resolution_kind,
    ModuleResolutionKind,
    MODULE_RESOLUTION_OPTION_MAP
);
float_or_int32_to_flag!(float_or_int32_to_script_target, ScriptTarget, TARGET_OPTION_MAP);
float_or_int32_to_flag!(float_or_int32_to_new_line_kind, NewLineKind, NEW_LINE_OPTION_MAP);

pub(crate) fn parse_type_acquisition(
    key: &str,
    value: &CompilerOptionsValue,
    all_options: &mut TypeAcquisition,
) -> Vec<P<Diagnostic>> {
    if value.is_null() {
        return Vec::new();
    }
    match key {
        "enable" => all_options.enable = parse_tristate(value),
        "include" => all_options.include = parse_string_array(value),
        "exclude" => all_options.exclude = parse_string_array(value),
        "disableFilenameBasedTypeAcquisition" => {
            all_options.disable_filename_based_type_acquisition = parse_tristate(value);
        }
        _ => {}
    }
    Vec::new()
}

fn parse_build_options(
    key: &str,
    value: &CompilerOptionsValue,
    all_options: &mut BuildOptions,
) -> Vec<P<Diagnostic>> {
    if value.is_null() {
        return Vec::new();
    }
    let option = BUILD_NAME_MAP.get(key);
    let key = match option {
        Some(option) => option.name,
        None => key,
    };
    match key {
        "clean" => all_options.clean = parse_tristate(value),
        "dry" => all_options.dry = parse_tristate(value),
        "force" => all_options.force = parse_tristate(value),
        "builders" => all_options.builders = parse_number_int(value),
        "stopBuildOnErrors" => all_options.stop_build_on_errors = parse_tristate(value),
        "verbose" => all_options.verbose = parse_tristate(value),
        _ => {}
    }
    Vec::new()
}

// reflect.Value.IsZero for the field types of core.CompilerOptions.
pub trait IsZero {
    fn is_zero(&self) -> bool;
}

impl IsZero for Tristate {
    fn is_zero(&self) -> bool {
        *self == Tristate::Unknown
    }
}

impl IsZero for String {
    fn is_zero(&self) -> bool {
        self.is_empty()
    }
}

impl<T> IsZero for Option<T> {
    fn is_zero(&self) -> bool {
        self.is_none()
    }
}

impl<T> IsZero for Vec<T> {
    fn is_zero(&self) -> bool {
        self.is_empty()
    }
}

macro_rules! impl_is_zero_for_enum {
    ($($ty:ty),*) => {
        $(impl IsZero for $ty {
            fn is_zero(&self) -> bool {
                *self == <$ty>::default()
            }
        })*
    };
}

impl_is_zero_for_enum!(
    JsxEmit,
    ModuleKind,
    ModuleResolutionKind,
    ModuleDetectionKind,
    NewLineKind,
    ScriptTarget
);

// Every exported field of core.CompilerOptions in declaration order, with its JSON name.
#[macro_export]
macro_rules! for_each_compiler_options_field {
    ($m:ident) => {
        $m! {
            allow_js: "allowJs",
            allow_arbitrary_extensions: "allowArbitraryExtensions",
            allow_importing_ts_extensions: "allowImportingTsExtensions",
            allow_non_ts_extensions: "allowNonTsExtensions",
            allow_umd_global_access: "allowUmdGlobalAccess",
            allow_unreachable_code: "allowUnreachableCode",
            allow_unused_labels: "allowUnusedLabels",
            assume_changes_only_affect_direct_dependencies: "assumeChangesOnlyAffectDirectDependencies",
            check_js: "checkJs",
            custom_conditions: "customConditions",
            composite: "composite",
            emit_declaration_only: "emitDeclarationOnly",
            emit_bom: "emitBOM",
            emit_decorator_metadata: "emitDecoratorMetadata",
            declaration: "declaration",
            declaration_dir: "declarationDir",
            declaration_map: "declarationMap",
            deduplicate_packages: "deduplicatePackages",
            disable_size_limit: "disableSizeLimit",
            disable_source_of_project_reference_redirect: "disableSourceOfProjectReferenceRedirect",
            disable_solution_searching: "disableSolutionSearching",
            disable_referenced_project_load: "disableReferencedProjectLoad",
            erasable_syntax_only: "erasableSyntaxOnly",
            exact_optional_property_types: "exactOptionalPropertyTypes",
            experimental_decorators: "experimentalDecorators",
            force_consistent_casing_in_file_names: "forceConsistentCasingInFileNames",
            isolated_modules: "isolatedModules",
            isolated_declarations: "isolatedDeclarations",
            ignore_config: "ignoreConfig",
            ignore_deprecations: "ignoreDeprecations",
            import_helpers: "importHelpers",
            inline_source_map: "inlineSourceMap",
            inline_sources: "inlineSources",
            init: "init",
            incremental: "incremental",
            jsx: "jsx",
            jsx_factory: "jsxFactory",
            jsx_fragment_factory: "jsxFragmentFactory",
            jsx_import_source: "jsxImportSource",
            lib: "lib",
            lib_replacement: "libReplacement",
            locale: "locale",
            map_root: "mapRoot",
            module: "module",
            module_resolution: "moduleResolution",
            module_suffixes: "moduleSuffixes",
            module_detection: "moduleDetection",
            new_line: "newLine",
            no_emit: "noEmit",
            no_check: "noCheck",
            no_error_truncation: "noErrorTruncation",
            no_fallthrough_cases_in_switch: "noFallthroughCasesInSwitch",
            no_implicit_any: "noImplicitAny",
            no_implicit_this: "noImplicitThis",
            no_implicit_returns: "noImplicitReturns",
            no_emit_helpers: "noEmitHelpers",
            no_lib: "noLib",
            no_property_access_from_index_signature: "noPropertyAccessFromIndexSignature",
            no_unchecked_indexed_access: "noUncheckedIndexedAccess",
            no_emit_on_error: "noEmitOnError",
            no_unused_locals: "noUnusedLocals",
            no_unused_parameters: "noUnusedParameters",
            no_resolve: "noResolve",
            no_implicit_override: "noImplicitOverride",
            no_unchecked_side_effect_imports: "noUncheckedSideEffectImports",
            out_dir: "outDir",
            paths: "paths",
            plugins: "plugins",
            preserve_const_enums: "preserveConstEnums",
            preserve_symlinks: "preserveSymlinks",
            project: "project",
            resolve_json_module: "resolveJsonModule",
            resolve_package_json_exports: "resolvePackageJsonExports",
            resolve_package_json_imports: "resolvePackageJsonImports",
            remove_comments: "removeComments",
            rewrite_relative_import_extensions: "rewriteRelativeImportExtensions",
            react_namespace: "reactNamespace",
            root_dir: "rootDir",
            root_dirs: "rootDirs",
            skip_lib_check: "skipLibCheck",
            stable_type_ordering: "stableTypeOrdering",
            strict: "strict",
            strict_bind_call_apply: "strictBindCallApply",
            strict_builtin_iterator_return: "strictBuiltinIteratorReturn",
            strict_function_types: "strictFunctionTypes",
            strict_null_checks: "strictNullChecks",
            strict_property_initialization: "strictPropertyInitialization",
            strip_internal: "stripInternal",
            skip_default_lib_check: "skipDefaultLibCheck",
            source_map: "sourceMap",
            source_root: "sourceRoot",
            suppress_output_path_check: "suppressOutputPathCheck",
            target: "target",
            trace_resolution: "traceResolution",
            ts_build_info_file: "tsBuildInfoFile",
            type_roots: "typeRoots",
            types: "types",
            use_define_for_class_fields: "useDefineForClassFields",
            use_unknown_in_catch_variables: "useUnknownInCatchVariables",
            verbatim_module_syntax: "verbatimModuleSyntax",
            max_node_module_js_depth: "maxNodeModuleJsDepth",
            allow_synthetic_default_imports: "allowSyntheticDefaultImports",
            always_strict: "alwaysStrict",
            base_url: "baseUrl",
            downlevel_iteration: "downlevelIteration",
            es_module_interop: "esModuleInterop",
            out_file: "outFile",
            config_file_path: "configFilePath",
            no_dts_resolution: "noDtsResolution",
            paths_base_path: "pathsBasePath",
            diagnostics: "diagnostics",
            extended_diagnostics: "extendedDiagnostics",
            generate_cpu_profile: "generateCpuProfile",
            generate_trace: "generateTrace",
            list_emitted_files: "listEmittedFiles",
            list_files: "listFiles",
            explain_files: "explainFiles",
            list_files_only: "listFilesOnly",
            no_emit_for_js_files: "noEmitForJsFiles",
            preserve_watch_output: "preserveWatchOutput",
            pretty: "pretty",
            version: "version",
            watch: "watch",
            show_config: "showConfig",
            build: "build",
            help: "help",
            all: "all",
            run_external_code: "runExternalCode",
            pprof_dir: "pprofDir",
            single_threaded: "singleThreaded",
            quiet: "quiet",
            checkers: "checkers",
        }
    };
}

pub use crate::for_each_compiler_options_field;

// A field of core.CompilerOptions seen through reflection (Go reflect.Value): IsZero and Interface.
pub trait OptionFieldValue {
    fn is_zero(&self) -> bool;
    fn interface(&self) -> CompilerOptionsValue;
}

impl<T: IsZero + ToOptionsValue> OptionFieldValue for T {
    fn is_zero(&self) -> bool {
        IsZero::is_zero(self)
    }
    fn interface(&self) -> CompilerOptionsValue {
        self.to_options_value()
    }
}

pub trait ToOptionsValue {
    fn to_options_value(&self) -> CompilerOptionsValue;
}

impl ToOptionsValue for Tristate {
    fn to_options_value(&self) -> CompilerOptionsValue {
        CompilerOptionsValue::Tristate(*self)
    }
}

impl ToOptionsValue for String {
    fn to_options_value(&self) -> CompilerOptionsValue {
        CompilerOptionsValue::String(self.clone())
    }
}

impl ToOptionsValue for Option<Vec<String>> {
    fn to_options_value(&self) -> CompilerOptionsValue {
        match self {
            Some(v) => CompilerOptionsValue::StringArray(v.clone()),
            None => CompilerOptionsValue::Null,
        }
    }
}

impl ToOptionsValue for Option<i64> {
    fn to_options_value(&self) -> CompilerOptionsValue {
        match self {
            Some(v) => CompilerOptionsValue::Int(*v),
            None => CompilerOptionsValue::Null,
        }
    }
}

impl ToOptionsValue for Option<i32> {
    fn to_options_value(&self) -> CompilerOptionsValue {
        match self {
            Some(v) => CompilerOptionsValue::Int(*v as i64),
            None => CompilerOptionsValue::Null,
        }
    }
}

impl ToOptionsValue for Option<OrderedMap<String, Option<Vec<String>>>> {
    fn to_options_value(&self) -> CompilerOptionsValue {
        match self {
            Some(m) => CompilerOptionsValue::Object(
                m.iter()
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            CompilerOptionsValue::StringArray(v.clone().unwrap_or_default()),
                        )
                    })
                    .collect(),
            ),
            None => CompilerOptionsValue::Null,
        }
    }
}

impl ToOptionsValue for Option<std::sync::Arc<OrderedMap<String, Option<Vec<String>>>>> {
    fn to_options_value(&self) -> CompilerOptionsValue {
        match self {
            Some(m) => CompilerOptionsValue::Object(
                m.iter()
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            CompilerOptionsValue::StringArray(v.clone().unwrap_or_default()),
                        )
                    })
                    .collect(),
            ),
            None => CompilerOptionsValue::Null,
        }
    }
}

impl ToOptionsValue for Option<Vec<PluginImport>> {
    fn to_options_value(&self) -> CompilerOptionsValue {
        match self {
            Some(v) => CompilerOptionsValue::Array(
                v.iter()
                    .map(|p| {
                        let mut m = OrderedMap::default();
                        m.insert("name".to_string(), CompilerOptionsValue::String(p.name.clone()));
                        CompilerOptionsValue::Object(m)
                    })
                    .collect(),
            ),
            None => CompilerOptionsValue::Null,
        }
    }
}

macro_rules! impl_to_options_value_for_enum {
    ($($ty:ident),*) => {
        $(impl ToOptionsValue for $ty {
            fn to_options_value(&self) -> CompilerOptionsValue {
                CompilerOptionsValue::$ty(*self)
            }
        })*
    };
}

impl_to_options_value_for_enum!(
    JsxEmit,
    ModuleKind,
    ModuleResolutionKind,
    ModuleDetectionKind,
    NewLineKind,
    ScriptTarget
);

// declscompiler.go:1234
// Go walks the struct fields by reflection; `i` is the field index.
pub fn for_each_compiler_option_value(
    options: &CompilerOptions,
    decl_filter: impl Fn(&CommandLineOption) -> bool,
    mut f: impl FnMut(&CommandLineOption, &dyn OptionFieldValue, usize) -> bool,
) -> bool {
    let mut i = 0usize;
    macro_rules! visit_fields {
        ($($field:ident: $json:literal,)*) => {
            $(
                if let Some(option_declaration) = COMMAND_LINE_COMPILER_OPTIONS_MAP.get($json) {
                    if decl_filter(option_declaration) {
                        if f(option_declaration, &options.$field, i) {
                            return true;
                        }
                    }
                }
                i += 1;
            )*
        };
    }
    for_each_compiler_options_field!(visit_fields);
    let _ = i;
    false
}

// mergeCompilerOptions merges the source compiler options into the target compiler options
// with optional awareness of explicitly set null values in the raw JSON.
// Fields in the source options will overwrite the corresponding fields in the target options,
// including when they are explicitly set to null in the raw configuration (if rawSource is provided).
pub(crate) fn merge_compiler_options(
    target_options: &mut CompilerOptions,
    source_options: Option<&CompilerOptions>,
    raw_source: Option<&CompilerOptionsValue>,
) {
    let Some(source_options) = source_options else {
        return;
    };

    // Collect explicitly null field names from raw JSON
    let mut explicit_null_fields: FxHashSet<&str> = FxHashSet::default();
    if let Some(CompilerOptionsValue::Object(raw_map)) = raw_source {
        // Options are nested under "compilerOptions" in both tsconfig.json and wrapped command line options
        if let Some(CompilerOptionsValue::Object(compiler_options_map)) =
            raw_map.get("compilerOptions")
        {
            explicit_null_fields.extend(compiler_options_map.iter().filter_map(|(key, value)| {
                if value.is_null() { Some(key.as_str()) } else { None }
            }));
        }
    }

    // Do the merge, handling explicit nulls during the normal merge
    macro_rules! merge_fields {
        ($($field:ident: $json:literal,)*) => {
            $(
                // Get the JSON field name for this struct field and check if it's explicitly null
                if explicit_null_fields.contains($json) {
                    target_options.$field = Default::default();
                } else if !IsZero::is_zero(&source_options.$field) {
                    // Normal merge behavior: copy non-zero fields
                    target_options.$field = source_options.$field.clone();
                }
            )*
        };
    }
    for_each_compiler_options_field!(merge_fields);
}

pub(crate) fn convert_to_options_with_absolute_paths(
    mut options_base: OrderedMap<String, CompilerOptionsValue>,
    option_map: &CommandLineOptionNameMap,
    cwd: &str,
) -> OrderedMap<String, CompilerOptionsValue> {
    // !!! convert to options with absolute paths was previously done with `CompilerOptions` object, but for ease of implementation, we do it pre-conversion.
    // !!! Revisit this choice if/when refactoring when conversion is done in tsconfig parsing
    let keys: Vec<String> = options_base.keys().cloned().collect();
    for o in keys {
        let v = options_base.get(&o).unwrap();
        if let Some(result) = convert_option_to_absolute_path(&o, v, option_map, cwd) {
            options_base.insert(o, result);
        }
    }
    options_base
}

pub fn convert_option_to_absolute_path(
    o: &str,
    v: &CompilerOptionsValue,
    option_map: &CommandLineOptionNameMap,
    cwd: &str,
) -> Option<CompilerOptionsValue> {
    let option = option_map.get(o)?;
    if option.kind == CommandLineOptionKind::List {
        if option.elements().unwrap().is_file_path {
            if let CompilerOptionsValue::StringArray(arr) = v {
                return Some(CompilerOptionsValue::StringArray(
                    arr.iter()
                        .map(|item| tspath::get_normalized_absolute_path(item, cwd))
                        .collect(),
                ));
            }
            if let CompilerOptionsValue::Array(arr) = v {
                return Some(CompilerOptionsValue::Array(
                    arr.iter()
                        .map(|item| {
                            if let CompilerOptionsValue::String(s) = item {
                                return CompilerOptionsValue::String(
                                    tspath::get_normalized_absolute_path(s, cwd),
                                );
                            }
                            item.clone()
                        })
                        .collect(),
                ));
            }
        }
    } else if option.is_file_path
        && let CompilerOptionsValue::String(value) = v
    {
        return Some(CompilerOptionsValue::String(tspath::get_normalized_absolute_path(
            value, cwd,
        )));
    }
    None
}
