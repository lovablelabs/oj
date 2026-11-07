use std::sync::LazyLock;

use tsrs_core::{ScriptTarget, Tristate};
use tsrs_diagnostics as diagnostics;

use crate::commandlineoption::{
    CommandLineOption, CommandLineOptionKind, DefaultValueDescription, ExtraValidation,
};
use crate::parsinghelpers::for_each_compiler_options_field;
use crate::tsconfigparsing::COMMAND_LINE_COMPILER_OPTIONS_MAP;
use tsrs_core::CompilerOptions;

pub static OPTIONS_DECLARATIONS: LazyLock<Vec<&'static CommandLineOption>> = LazyLock::new(|| {
    COMMON_OPTIONS_WITH_BUILD.iter().chain(OPTIONS_FOR_COMPILER.iter()).copied().collect()
});

// BEGIN GENERATED
static COMMON_OPTIONS_WITH_BUILD_ITEMS: [CommandLineOption; 29] = [
    CommandLineOption {
        name: "help",
        short_name: "h",
        kind: CommandLineOptionKind::Boolean,
        show_in_simplified_help_view: true,
        is_command_line_only: true,
        category: Some(&diagnostics::COMMAND_LINE_OPTIONS),
        description: Some(&diagnostics::PRINT_THIS_MESSAGE),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "help",
        short_name: "?",
        kind: CommandLineOptionKind::Boolean,
        is_command_line_only: true,
        category: Some(&diagnostics::COMMAND_LINE_OPTIONS),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "watch",
        short_name: "w",
        kind: CommandLineOptionKind::Boolean,
        show_in_simplified_help_view: true,
        is_command_line_only: true,
        category: Some(&diagnostics::COMMAND_LINE_OPTIONS),
        description: Some(&diagnostics::WATCH_INPUT_FILES),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "preserveWatchOutput",
        kind: CommandLineOptionKind::Boolean,
        show_in_simplified_help_view: false,
        category: Some(&diagnostics::OUTPUT_FORMATTING),
        description: Some(&diagnostics::DISABLE_WIPING_THE_CONSOLE_IN_WATCH_MODE),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "listFiles",
        kind: CommandLineOptionKind::Boolean,
        category: Some(&diagnostics::COMPILER_DIAGNOSTICS),
        description: Some(&diagnostics::PRINT_ALL_OF_THE_FILES_READ_DURING_THE_COMPILATION),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "explainFiles",
        kind: CommandLineOptionKind::Boolean,
        category: Some(&diagnostics::COMPILER_DIAGNOSTICS),
        description: Some(&diagnostics::PRINT_FILES_READ_DURING_THE_COMPILATION_INCLUDING_WHY_IT_WAS_INCLUDED),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "listEmittedFiles",
        kind: CommandLineOptionKind::Boolean,
        category: Some(&diagnostics::COMPILER_DIAGNOSTICS),
        description: Some(&diagnostics::PRINT_THE_NAMES_OF_EMITTED_FILES_AFTER_A_COMPILATION),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "pretty",
        kind: CommandLineOptionKind::Boolean,
        show_in_simplified_help_view: true,
        category: Some(&diagnostics::OUTPUT_FORMATTING),
        description: Some(&diagnostics::ENABLE_COLOR_AND_FORMATTING_IN_TYPE_SCRIPT_S_OUTPUT_TO_MAKE_COMPILER_ERRORS_EASIER_TO_READ),
        default_value_description: DefaultValueDescription::Bool(true),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "traceResolution",
        kind: CommandLineOptionKind::Boolean,
        category: Some(&diagnostics::COMPILER_DIAGNOSTICS),
        description: Some(&diagnostics::LOG_PATHS_USED_DURING_THE_MODULE_RESOLUTION_PROCESS),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "diagnostics",
        kind: CommandLineOptionKind::Boolean,
        category: Some(&diagnostics::COMPILER_DIAGNOSTICS),
        description: Some(&diagnostics::OUTPUT_COMPILER_PERFORMANCE_INFORMATION_AFTER_BUILDING),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "extendedDiagnostics",
        kind: CommandLineOptionKind::Boolean,
        category: Some(&diagnostics::COMPILER_DIAGNOSTICS),
        description: Some(&diagnostics::OUTPUT_MORE_DETAILED_COMPILER_PERFORMANCE_INFORMATION_AFTER_BUILDING),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "generateCpuProfile",
        kind: CommandLineOptionKind::String,
        is_file_path: true,
        category: Some(&diagnostics::COMPILER_DIAGNOSTICS),
        description: Some(&diagnostics::EMIT_A_V8_CPU_PROFILE_OF_THE_COMPILER_RUN_FOR_DEBUGGING),
        default_value_description: DefaultValueDescription::Str("profile.cpuprofile"),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "generateTrace",
        kind: CommandLineOptionKind::String,
        is_file_path: true,
        category: Some(&diagnostics::COMPILER_DIAGNOSTICS),
        description: Some(&diagnostics::GENERATES_AN_EVENT_TRACE_AND_A_LIST_OF_TYPES),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "incremental",
        short_name: "i",
        kind: CommandLineOptionKind::Boolean,
        category: Some(&diagnostics::PROJECTS),
        description: Some(&diagnostics::SAVE_TSBUILDINFO_FILES_TO_ALLOW_FOR_INCREMENTAL_COMPILATION_OF_PROJECTS),
        transpile_option_value: Tristate::Unknown,
        default_value_description: DefaultValueDescription::Message(&diagnostics::X_FALSE_UNLESS_COMPOSITE_IS_SET),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "declaration",
        short_name: "d",
        kind: CommandLineOptionKind::Boolean,
        affects_build_info: true,
        show_in_simplified_help_view: true,
        category: Some(&diagnostics::EMIT),
        transpile_option_value: Tristate::Unknown,
        description: Some(&diagnostics::GENERATE_D_TS_FILES_FROM_TYPE_SCRIPT_AND_JAVA_SCRIPT_FILES_IN_YOUR_PROJECT),
        default_value_description: DefaultValueDescription::Message(&diagnostics::X_FALSE_UNLESS_COMPOSITE_IS_SET),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "declarationMap",
        kind: CommandLineOptionKind::Boolean,
        affects_build_info: true,
        show_in_simplified_help_view: true,
        category: Some(&diagnostics::EMIT),
        default_value_description: DefaultValueDescription::Bool(false),
        description: Some(&diagnostics::CREATE_SOURCEMAPS_FOR_D_TS_FILES),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "emitDeclarationOnly",
        kind: CommandLineOptionKind::Boolean,
        affects_build_info: true,
        show_in_simplified_help_view: true,
        category: Some(&diagnostics::EMIT),
        description: Some(&diagnostics::ONLY_OUTPUT_D_TS_FILES_AND_NOT_JAVA_SCRIPT_FILES),
        transpile_option_value: Tristate::Unknown,
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "sourceMap",
        kind: CommandLineOptionKind::Boolean,
        affects_build_info: true,
        show_in_simplified_help_view: true,
        category: Some(&diagnostics::EMIT),
        default_value_description: DefaultValueDescription::Bool(false),
        description: Some(&diagnostics::CREATE_SOURCE_MAP_FILES_FOR_EMITTED_JAVA_SCRIPT_FILES),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "inlineSourceMap",
        kind: CommandLineOptionKind::Boolean,
        affects_build_info: true,
        category: Some(&diagnostics::EMIT),
        description: Some(&diagnostics::INCLUDE_SOURCEMAP_FILES_INSIDE_THE_EMITTED_JAVA_SCRIPT),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "noCheck",
        kind: CommandLineOptionKind::Boolean,
        show_in_simplified_help_view: false,
        category: Some(&diagnostics::COMPILER_DIAGNOSTICS),
        description: Some(&diagnostics::DISABLE_FULL_TYPE_CHECKING_ONLY_CRITICAL_PARSE_AND_EMIT_ERRORS_WILL_BE_REPORTED),
        transpile_option_value: Tristate::True,
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "deduplicatePackages",
        kind: CommandLineOptionKind::Boolean,
        category: Some(&diagnostics::TYPE_CHECKING),
        description: Some(&diagnostics::DEDUPLICATE_PACKAGES_WITH_THE_SAME_NAME_AND_VERSION),
        default_value_description: DefaultValueDescription::Bool(true),
        affects_program_structure: true,
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "noEmit",
        kind: CommandLineOptionKind::Boolean,
        show_in_simplified_help_view: true,
        category: Some(&diagnostics::EMIT),
        description: Some(&diagnostics::DISABLE_EMITTING_FILES_FROM_A_COMPILATION),
        transpile_option_value: Tristate::Unknown,
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "assumeChangesOnlyAffectDirectDependencies",
        kind: CommandLineOptionKind::Boolean,
        affects_semantic_diagnostics: true,
        affects_emit: true,
        affects_build_info: true,
        category: Some(&diagnostics::WATCH_AND_BUILD_MODES),
        description: Some(&diagnostics::HAVE_RECOMPILES_IN_PROJECTS_THAT_USE_INCREMENTAL_AND_WATCH_MODE_ASSUME_THAT_CHANGES_WITHIN_A_FILE_WILL_ONLY_AFFECT_FILES_DIRECTLY_DEPENDING_ON_IT),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "locale",
        kind: CommandLineOptionKind::String,
        category: Some(&diagnostics::COMMAND_LINE_OPTIONS),
        is_command_line_only: true,
        description: Some(&diagnostics::SET_THE_LANGUAGE_OF_THE_MESSAGING_FROM_TYPE_SCRIPT_THIS_DOES_NOT_AFFECT_EMIT),
        default_value_description: DefaultValueDescription::Message(&diagnostics::PLATFORM_SPECIFIC),
        extra_validation: ExtraValidation::Locale,
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "quiet",
        short_name: "q",
        kind: CommandLineOptionKind::Boolean,
        category: Some(&diagnostics::COMMAND_LINE_OPTIONS),
        description: Some(&diagnostics::DO_NOT_PRINT_DIAGNOSTICS),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "singleThreaded",
        kind: CommandLineOptionKind::Boolean,
        category: Some(&diagnostics::COMMAND_LINE_OPTIONS),
        description: Some(&diagnostics::RUN_IN_SINGLE_THREADED_MODE),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "pprofDir",
        kind: CommandLineOptionKind::String,
        is_file_path: true,
        category: Some(&diagnostics::COMMAND_LINE_OPTIONS),
        description: Some(&diagnostics::GENERATE_PPROF_CPU_SLASHMEMORY_PROFILES_TO_THE_GIVEN_DIRECTORY),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "checkers",
        kind: CommandLineOptionKind::Number,
        category: Some(&diagnostics::COMMAND_LINE_OPTIONS),
        description: Some(&diagnostics::SET_THE_NUMBER_OF_CHECKERS_PER_PROJECT),
        default_value_description: DefaultValueDescription::Message(&diagnostics::X_4_UNLESS_SINGLE_THREADED_IS_PASSED),
        min_value: 1,
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "runExternalCode",
        kind: CommandLineOptionKind::Boolean,
        category: Some(&diagnostics::COMMAND_LINE_OPTIONS),
        is_command_line_only: true,
        description: Some(&diagnostics::ALLOW_LOADING_EXTERNAL_CONTENT_MAPPER_PLUGINS_THAT_EXECUTE_CODE_DURING_COMPILATION),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
];

pub(crate) static COMMON_OPTIONS_WITH_BUILD: LazyLock<Vec<&'static CommandLineOption>> =
    LazyLock::new(|| {
        vec![
            &COMMON_OPTIONS_WITH_BUILD_ITEMS[0],
            &COMMON_OPTIONS_WITH_BUILD_ITEMS[1],
            &COMMON_OPTIONS_WITH_BUILD_ITEMS[2],
            &COMMON_OPTIONS_WITH_BUILD_ITEMS[3],
            &COMMON_OPTIONS_WITH_BUILD_ITEMS[4],
            &COMMON_OPTIONS_WITH_BUILD_ITEMS[5],
            &COMMON_OPTIONS_WITH_BUILD_ITEMS[6],
            &COMMON_OPTIONS_WITH_BUILD_ITEMS[7],
            &COMMON_OPTIONS_WITH_BUILD_ITEMS[8],
            &COMMON_OPTIONS_WITH_BUILD_ITEMS[9],
            &COMMON_OPTIONS_WITH_BUILD_ITEMS[10],
            &COMMON_OPTIONS_WITH_BUILD_ITEMS[11],
            &COMMON_OPTIONS_WITH_BUILD_ITEMS[12],
            &COMMON_OPTIONS_WITH_BUILD_ITEMS[13],
            &COMMON_OPTIONS_WITH_BUILD_ITEMS[14],
            &COMMON_OPTIONS_WITH_BUILD_ITEMS[15],
            &COMMON_OPTIONS_WITH_BUILD_ITEMS[16],
            &COMMON_OPTIONS_WITH_BUILD_ITEMS[17],
            &COMMON_OPTIONS_WITH_BUILD_ITEMS[18],
            &COMMON_OPTIONS_WITH_BUILD_ITEMS[19],
            &COMMON_OPTIONS_WITH_BUILD_ITEMS[20],
            &COMMON_OPTIONS_WITH_BUILD_ITEMS[21],
            &COMMON_OPTIONS_WITH_BUILD_ITEMS[22],
            &COMMON_OPTIONS_WITH_BUILD_ITEMS[23],
            &COMMON_OPTIONS_WITH_BUILD_ITEMS[24],
            &COMMON_OPTIONS_WITH_BUILD_ITEMS[25],
            &COMMON_OPTIONS_WITH_BUILD_ITEMS[26],
            &COMMON_OPTIONS_WITH_BUILD_ITEMS[27],
            &COMMON_OPTIONS_WITH_BUILD_ITEMS[28],
        ]
    });

static OPTIONS_FOR_COMPILER_ITEMS: [CommandLineOption; 97] = [
    CommandLineOption {
        name: "all",
        kind: CommandLineOptionKind::Boolean,
        show_in_simplified_help_view: true,
        category: Some(&diagnostics::COMMAND_LINE_OPTIONS),
        description: Some(&diagnostics::SHOW_ALL_COMPILER_OPTIONS),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "version",
        short_name: "v",
        kind: CommandLineOptionKind::Boolean,
        show_in_simplified_help_view: true,
        category: Some(&diagnostics::COMMAND_LINE_OPTIONS),
        description: Some(&diagnostics::PRINT_THE_COMPILER_S_VERSION),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "init",
        kind: CommandLineOptionKind::Boolean,
        show_in_simplified_help_view: true,
        category: Some(&diagnostics::COMMAND_LINE_OPTIONS),
        description: Some(&diagnostics::INITIALIZES_A_TYPE_SCRIPT_PROJECT_AND_CREATES_A_TSCONFIG_JSON_FILE),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "project",
        short_name: "p",
        kind: CommandLineOptionKind::String,
        is_file_path: true,
        show_in_simplified_help_view: true,
        category: Some(&diagnostics::COMMAND_LINE_OPTIONS),
        description: Some(&diagnostics::COMPILE_THE_PROJECT_GIVEN_THE_PATH_TO_ITS_CONFIGURATION_FILE_OR_TO_A_FOLDER_WITH_A_TSCONFIG_JSON),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "showConfig",
        kind: CommandLineOptionKind::Boolean,
        show_in_simplified_help_view: true,
        category: Some(&diagnostics::COMMAND_LINE_OPTIONS),
        is_command_line_only: true,
        description: Some(&diagnostics::PRINT_THE_FINAL_CONFIGURATION_INSTEAD_OF_BUILDING),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "listFilesOnly",
        kind: CommandLineOptionKind::Boolean,
        category: Some(&diagnostics::COMMAND_LINE_OPTIONS),
        is_command_line_only: true,
        description: Some(&diagnostics::PRINT_NAMES_OF_FILES_THAT_ARE_PART_OF_THE_COMPILATION_AND_THEN_STOP_PROCESSING),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "ignoreConfig",
        kind: CommandLineOptionKind::Boolean,
        show_in_simplified_help_view: true,
        category: Some(&diagnostics::COMMAND_LINE_OPTIONS),
        is_command_line_only: true,
        description: Some(&diagnostics::IGNORE_THE_TSCONFIG_FOUND_AND_BUILD_WITH_COMMANDLINE_OPTIONS_AND_FILES),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "target",
        short_name: "t",
        kind: CommandLineOptionKind::Enum,
        affects_source_file: true,
        affects_module_resolution: true,
        affects_emit: true,
        affects_build_info: true,
        show_in_simplified_help_view: true,
        category: Some(&diagnostics::LANGUAGE_AND_ENVIRONMENT),
        description: Some(&diagnostics::SET_THE_JAVA_SCRIPT_LANGUAGE_VERSION_FOR_EMITTED_JAVA_SCRIPT_AND_INCLUDE_COMPATIBLE_LIBRARY_DECLARATIONS),
        default_value_description: DefaultValueDescription::ScriptTarget(ScriptTarget::LATEST_STANDARD),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "module",
        short_name: "m",
        kind: CommandLineOptionKind::Enum,
        affects_module_resolution: true,
        affects_emit: true,
        affects_build_info: true,
        show_in_simplified_help_view: true,
        category: Some(&diagnostics::MODULES),
        description: Some(&diagnostics::SPECIFY_WHAT_MODULE_CODE_IS_GENERATED),
        default_value_description: DefaultValueDescription::Tristate(Tristate::Unknown),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "lib",
        kind: CommandLineOptionKind::List,
        affects_program_structure: true,
        show_in_simplified_help_view: true,
        category: Some(&diagnostics::LANGUAGE_AND_ENVIRONMENT),
        description: Some(&diagnostics::SPECIFY_A_SET_OF_BUNDLED_LIBRARY_DECLARATION_FILES_THAT_DESCRIBE_THE_TARGET_RUNTIME_ENVIRONMENT),
        transpile_option_value: Tristate::Unknown,
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "allowJs",
        kind: CommandLineOptionKind::Boolean,
        allow_js_flag: true,
        affects_build_info: true,
        show_in_simplified_help_view: true,
        category: Some(&diagnostics::JAVA_SCRIPT_SUPPORT),
        description: Some(&diagnostics::ALLOW_JAVA_SCRIPT_FILES_TO_BE_A_PART_OF_YOUR_PROGRAM_USE_THE_CHECK_JS_OPTION_TO_GET_ERRORS_FROM_THESE_FILES),
        default_value_description: DefaultValueDescription::Message(&diagnostics::X_FALSE_UNLESS_CHECK_JS_IS_SET),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "checkJs",
        kind: CommandLineOptionKind::Boolean,
        affects_module_resolution: true,
        affects_semantic_diagnostics: true,
        affects_build_info: true,
        show_in_simplified_help_view: true,
        category: Some(&diagnostics::JAVA_SCRIPT_SUPPORT),
        description: Some(&diagnostics::ENABLE_ERROR_REPORTING_IN_TYPE_CHECKED_JAVA_SCRIPT_FILES),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "jsx",
        kind: CommandLineOptionKind::Enum,
        affects_source_file: true,
        affects_emit: true,
        affects_build_info: true,
        affects_module_resolution: true,
        affects_semantic_diagnostics: true,
        show_in_simplified_help_view: true,
        category: Some(&diagnostics::LANGUAGE_AND_ENVIRONMENT),
        description: Some(&diagnostics::SPECIFY_WHAT_JSX_CODE_IS_GENERATED),
        default_value_description: DefaultValueDescription::Tristate(Tristate::Unknown),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "outFile",
        kind: CommandLineOptionKind::String,
        affects_emit: true,
        affects_build_info: true,
        affects_declaration_path: true,
        is_file_path: true,
        show_in_simplified_help_view: true,
        category: Some(&diagnostics::EMIT),
        description: Some(&diagnostics::SPECIFY_A_FILE_THAT_BUNDLES_ALL_OUTPUTS_INTO_ONE_JAVA_SCRIPT_FILE_IF_DECLARATION_IS_TRUE_ALSO_DESIGNATES_A_FILE_THAT_BUNDLES_ALL_D_TS_OUTPUT),
        transpile_option_value: Tristate::Unknown,
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "outDir",
        kind: CommandLineOptionKind::String,
        affects_emit: true,
        affects_build_info: true,
        affects_declaration_path: true,
        is_file_path: true,
        show_in_simplified_help_view: true,
        category: Some(&diagnostics::EMIT),
        description: Some(&diagnostics::SPECIFY_AN_OUTPUT_FOLDER_FOR_ALL_EMITTED_FILES),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "rootDir",
        kind: CommandLineOptionKind::String,
        affects_emit: true,
        affects_build_info: true,
        affects_declaration_path: true,
        is_file_path: true,
        category: Some(&diagnostics::MODULES),
        description: Some(&diagnostics::SPECIFY_THE_ROOT_FOLDER_WITHIN_YOUR_SOURCE_FILES),
        default_value_description: DefaultValueDescription::Message(&diagnostics::COMPUTED_FROM_THE_LIST_OF_INPUT_FILES),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "composite",
        kind: CommandLineOptionKind::Boolean,
        affects_build_info: true,
        is_tsconfig_only: true,
        category: Some(&diagnostics::PROJECTS),
        transpile_option_value: Tristate::Unknown,
        default_value_description: DefaultValueDescription::Bool(false),
        description: Some(&diagnostics::ENABLE_CONSTRAINTS_THAT_ALLOW_A_TYPE_SCRIPT_PROJECT_TO_BE_USED_WITH_PROJECT_REFERENCES),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "tsBuildInfoFile",
        kind: CommandLineOptionKind::String,
        affects_emit: true,
        affects_build_info: true,
        is_file_path: true,
        category: Some(&diagnostics::PROJECTS),
        transpile_option_value: Tristate::Unknown,
        default_value_description: DefaultValueDescription::Str(".tsbuildinfo"),
        description: Some(&diagnostics::SPECIFY_THE_PATH_TO_TSBUILDINFO_INCREMENTAL_COMPILATION_FILE),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "removeComments",
        kind: CommandLineOptionKind::Boolean,
        affects_emit: true,
        affects_build_info: true,
        show_in_simplified_help_view: true,
        category: Some(&diagnostics::EMIT),
        default_value_description: DefaultValueDescription::Bool(false),
        description: Some(&diagnostics::DISABLE_EMITTING_COMMENTS),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "importHelpers",
        kind: CommandLineOptionKind::Boolean,
        affects_emit: true,
        affects_build_info: true,
        affects_source_file: true,
        category: Some(&diagnostics::EMIT),
        description: Some(&diagnostics::ALLOW_IMPORTING_HELPER_FUNCTIONS_FROM_TSLIB_ONCE_PER_PROJECT_INSTEAD_OF_INCLUDING_THEM_PER_FILE),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "downlevelIteration",
        kind: CommandLineOptionKind::Boolean,
        affects_emit: true,
        affects_build_info: true,
        category: Some(&diagnostics::EMIT),
        description: Some(&diagnostics::EMIT_MORE_COMPLIANT_BUT_VERBOSE_AND_LESS_PERFORMANT_JAVA_SCRIPT_FOR_ITERATION),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "isolatedModules",
        kind: CommandLineOptionKind::Boolean,
        category: Some(&diagnostics::INTEROP_CONSTRAINTS),
        description: Some(&diagnostics::ENSURE_THAT_EACH_FILE_CAN_BE_SAFELY_TRANSPILED_WITHOUT_RELYING_ON_OTHER_IMPORTS),
        transpile_option_value: Tristate::True,
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "verbatimModuleSyntax",
        kind: CommandLineOptionKind::Boolean,
        affects_emit: true,
        affects_semantic_diagnostics: true,
        affects_build_info: true,
        category: Some(&diagnostics::INTEROP_CONSTRAINTS),
        description: Some(&diagnostics::DO_NOT_TRANSFORM_OR_ELIDE_ANY_IMPORTS_OR_EXPORTS_NOT_MARKED_AS_TYPE_ONLY_ENSURING_THEY_ARE_WRITTEN_IN_THE_OUTPUT_FILE_S_FORMAT_BASED_ON_THE_MODULE_SETTING),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "isolatedDeclarations",
        kind: CommandLineOptionKind::Boolean,
        category: Some(&diagnostics::INTEROP_CONSTRAINTS),
        description: Some(&diagnostics::REQUIRE_SUFFICIENT_ANNOTATION_ON_EXPORTS_SO_OTHER_TOOLS_CAN_TRIVIALLY_GENERATE_DECLARATION_FILES),
        default_value_description: DefaultValueDescription::Bool(false),
        affects_build_info: true,
        affects_semantic_diagnostics: true,
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "erasableSyntaxOnly",
        kind: CommandLineOptionKind::Boolean,
        category: Some(&diagnostics::INTEROP_CONSTRAINTS),
        description: Some(&diagnostics::DO_NOT_ALLOW_RUNTIME_CONSTRUCTS_THAT_ARE_NOT_PART_OF_ECMA_SCRIPT),
        default_value_description: DefaultValueDescription::Bool(false),
        affects_build_info: true,
        affects_semantic_diagnostics: true,
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "libReplacement",
        kind: CommandLineOptionKind::Boolean,
        affects_program_structure: true,
        category: Some(&diagnostics::LANGUAGE_AND_ENVIRONMENT),
        description: Some(&diagnostics::ENABLE_LIB_REPLACEMENT),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "strict",
        kind: CommandLineOptionKind::Boolean,
        affects_build_info: true,
        show_in_simplified_help_view: true,
        category: Some(&diagnostics::TYPE_CHECKING),
        description: Some(&diagnostics::ENABLE_ALL_STRICT_TYPE_CHECKING_OPTIONS),
        default_value_description: DefaultValueDescription::Bool(true),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "noImplicitAny",
        kind: CommandLineOptionKind::Boolean,
        affects_semantic_diagnostics: true,
        affects_build_info: true,
        strict_flag: true,
        category: Some(&diagnostics::TYPE_CHECKING),
        description: Some(&diagnostics::ENABLE_ERROR_REPORTING_FOR_EXPRESSIONS_AND_DECLARATIONS_WITH_AN_IMPLIED_ANY_TYPE),
        default_value_description: DefaultValueDescription::Message(&diagnostics::X_TRUE_UNLESS_STRICT_IS_FALSE),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "strictNullChecks",
        kind: CommandLineOptionKind::Boolean,
        affects_semantic_diagnostics: true,
        affects_build_info: true,
        strict_flag: true,
        category: Some(&diagnostics::TYPE_CHECKING),
        description: Some(&diagnostics::WHEN_TYPE_CHECKING_TAKE_INTO_ACCOUNT_NULL_AND_UNDEFINED),
        default_value_description: DefaultValueDescription::Message(&diagnostics::X_TRUE_UNLESS_STRICT_IS_FALSE),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "strictFunctionTypes",
        kind: CommandLineOptionKind::Boolean,
        affects_semantic_diagnostics: true,
        affects_build_info: true,
        strict_flag: true,
        category: Some(&diagnostics::TYPE_CHECKING),
        description: Some(&diagnostics::WHEN_ASSIGNING_FUNCTIONS_CHECK_TO_ENSURE_PARAMETERS_AND_THE_RETURN_VALUES_ARE_SUBTYPE_COMPATIBLE),
        default_value_description: DefaultValueDescription::Message(&diagnostics::X_TRUE_UNLESS_STRICT_IS_FALSE),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "strictBindCallApply",
        kind: CommandLineOptionKind::Boolean,
        affects_semantic_diagnostics: true,
        affects_build_info: true,
        strict_flag: true,
        category: Some(&diagnostics::TYPE_CHECKING),
        description: Some(&diagnostics::CHECK_THAT_THE_ARGUMENTS_FOR_BIND_CALL_AND_APPLY_METHODS_MATCH_THE_ORIGINAL_FUNCTION),
        default_value_description: DefaultValueDescription::Message(&diagnostics::X_TRUE_UNLESS_STRICT_IS_FALSE),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "strictPropertyInitialization",
        kind: CommandLineOptionKind::Boolean,
        affects_semantic_diagnostics: true,
        affects_build_info: true,
        strict_flag: true,
        category: Some(&diagnostics::TYPE_CHECKING),
        description: Some(&diagnostics::CHECK_FOR_CLASS_PROPERTIES_THAT_ARE_DECLARED_BUT_NOT_SET_IN_THE_CONSTRUCTOR),
        default_value_description: DefaultValueDescription::Message(&diagnostics::X_TRUE_UNLESS_STRICT_IS_FALSE),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "strictBuiltinIteratorReturn",
        kind: CommandLineOptionKind::Boolean,
        affects_semantic_diagnostics: true,
        affects_build_info: true,
        strict_flag: true,
        category: Some(&diagnostics::TYPE_CHECKING),
        description: Some(&diagnostics::BUILT_IN_ITERATORS_ARE_INSTANTIATED_WITH_A_T_RETURN_TYPE_OF_UNDEFINED_INSTEAD_OF_ANY),
        default_value_description: DefaultValueDescription::Message(&diagnostics::X_TRUE_UNLESS_STRICT_IS_FALSE),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "noImplicitThis",
        kind: CommandLineOptionKind::Boolean,
        affects_semantic_diagnostics: true,
        affects_build_info: true,
        strict_flag: true,
        category: Some(&diagnostics::TYPE_CHECKING),
        description: Some(&diagnostics::ENABLE_ERROR_REPORTING_WHEN_THIS_IS_GIVEN_THE_TYPE_ANY),
        default_value_description: DefaultValueDescription::Message(&diagnostics::X_TRUE_UNLESS_STRICT_IS_FALSE),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "useUnknownInCatchVariables",
        kind: CommandLineOptionKind::Boolean,
        affects_semantic_diagnostics: true,
        affects_build_info: true,
        strict_flag: true,
        category: Some(&diagnostics::TYPE_CHECKING),
        description: Some(&diagnostics::DEFAULT_CATCH_CLAUSE_VARIABLES_AS_UNKNOWN_INSTEAD_OF_ANY),
        default_value_description: DefaultValueDescription::Message(&diagnostics::X_TRUE_UNLESS_STRICT_IS_FALSE),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "alwaysStrict",
        kind: CommandLineOptionKind::Boolean,
        affects_source_file: true,
        affects_emit: true,
        affects_build_info: true,
        category: Some(&diagnostics::TYPE_CHECKING),
        description: Some(&diagnostics::ENSURE_USE_STRICT_IS_ALWAYS_EMITTED),
        default_value_description: DefaultValueDescription::Bool(true),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "stableTypeOrdering",
        kind: CommandLineOptionKind::Boolean,
        affects_semantic_diagnostics: true,
        affects_build_info: true,
        category: Some(&diagnostics::TYPE_CHECKING),
        description: Some(&diagnostics::ENSURE_TYPES_ARE_ORDERED_STABLY_AND_DETERMINISTICALLY_ACROSS_COMPILATIONS),
        default_value_description: DefaultValueDescription::Bool(true),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "noUnusedLocals",
        kind: CommandLineOptionKind::Boolean,
        affects_semantic_diagnostics: true,
        affects_build_info: true,
        category: Some(&diagnostics::TYPE_CHECKING),
        description: Some(&diagnostics::ENABLE_ERROR_REPORTING_WHEN_LOCAL_VARIABLES_AREN_T_READ),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "noUnusedParameters",
        kind: CommandLineOptionKind::Boolean,
        affects_semantic_diagnostics: true,
        affects_build_info: true,
        category: Some(&diagnostics::TYPE_CHECKING),
        description: Some(&diagnostics::RAISE_AN_ERROR_WHEN_A_FUNCTION_PARAMETER_ISN_T_READ),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "exactOptionalPropertyTypes",
        kind: CommandLineOptionKind::Boolean,
        affects_semantic_diagnostics: true,
        affects_build_info: true,
        category: Some(&diagnostics::TYPE_CHECKING),
        description: Some(&diagnostics::INTERPRET_OPTIONAL_PROPERTY_TYPES_AS_WRITTEN_RATHER_THAN_ADDING_UNDEFINED),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "noImplicitReturns",
        kind: CommandLineOptionKind::Boolean,
        affects_semantic_diagnostics: true,
        affects_build_info: true,
        category: Some(&diagnostics::TYPE_CHECKING),
        description: Some(&diagnostics::ENABLE_ERROR_REPORTING_FOR_CODEPATHS_THAT_DO_NOT_EXPLICITLY_RETURN_IN_A_FUNCTION),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "noFallthroughCasesInSwitch",
        kind: CommandLineOptionKind::Boolean,
        affects_bind_diagnostics: true,
        affects_semantic_diagnostics: true,
        affects_build_info: true,
        category: Some(&diagnostics::TYPE_CHECKING),
        description: Some(&diagnostics::ENABLE_ERROR_REPORTING_FOR_FALLTHROUGH_CASES_IN_SWITCH_STATEMENTS),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "noUncheckedIndexedAccess",
        kind: CommandLineOptionKind::Boolean,
        affects_semantic_diagnostics: true,
        affects_build_info: true,
        category: Some(&diagnostics::TYPE_CHECKING),
        description: Some(&diagnostics::ADD_UNDEFINED_TO_A_TYPE_WHEN_ACCESSED_USING_AN_INDEX),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "noImplicitOverride",
        kind: CommandLineOptionKind::Boolean,
        affects_semantic_diagnostics: true,
        affects_build_info: true,
        category: Some(&diagnostics::TYPE_CHECKING),
        description: Some(&diagnostics::ENSURE_OVERRIDING_MEMBERS_IN_DERIVED_CLASSES_ARE_MARKED_WITH_AN_OVERRIDE_MODIFIER),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "noPropertyAccessFromIndexSignature",
        kind: CommandLineOptionKind::Boolean,
        affects_semantic_diagnostics: true,
        affects_build_info: true,
        show_in_simplified_help_view: false,
        category: Some(&diagnostics::TYPE_CHECKING),
        description: Some(&diagnostics::ENFORCES_USING_INDEXED_ACCESSORS_FOR_KEYS_DECLARED_USING_AN_INDEXED_TYPE),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "moduleResolution",
        kind: CommandLineOptionKind::Enum,
        affects_module_resolution: true,
        category: Some(&diagnostics::MODULES),
        description: Some(&diagnostics::SPECIFY_HOW_TYPE_SCRIPT_LOOKS_UP_A_FILE_FROM_A_GIVEN_MODULE_SPECIFIER),
        default_value_description: DefaultValueDescription::Message(&diagnostics::X_NODENEXT_IF_MODULE_IS_NODENEXT_NODE16_IF_MODULE_IS_NODE16_OR_NODE18_OTHERWISE_BUNDLER),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "baseUrl",
        kind: CommandLineOptionKind::String,
        affects_module_resolution: true,
        is_file_path: true,
        category: Some(&diagnostics::MODULES),
        description: Some(&diagnostics::SPECIFY_THE_BASE_DIRECTORY_TO_RESOLVE_NON_RELATIVE_MODULE_NAMES),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "paths",
        kind: CommandLineOptionKind::Object,
        affects_module_resolution: true,
        allow_config_dir_template_substitution: true,
        is_tsconfig_only: true,
        category: Some(&diagnostics::MODULES),
        description: Some(&diagnostics::SPECIFY_A_SET_OF_ENTRIES_THAT_RE_MAP_IMPORTS_TO_ADDITIONAL_LOOKUP_LOCATIONS),
        transpile_option_value: Tristate::Unknown,
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "rootDirs",
        kind: CommandLineOptionKind::List,
        is_tsconfig_only: true,
        affects_module_resolution: true,
        allow_config_dir_template_substitution: true,
        category: Some(&diagnostics::MODULES),
        description: Some(&diagnostics::ALLOW_MULTIPLE_FOLDERS_TO_BE_TREATED_AS_ONE_WHEN_RESOLVING_MODULES),
        transpile_option_value: Tristate::Unknown,
        default_value_description: DefaultValueDescription::Message(&diagnostics::COMPUTED_FROM_THE_LIST_OF_INPUT_FILES),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "typeRoots",
        kind: CommandLineOptionKind::List,
        affects_module_resolution: true,
        allow_config_dir_template_substitution: true,
        category: Some(&diagnostics::MODULES),
        description: Some(&diagnostics::SPECIFY_MULTIPLE_FOLDERS_THAT_ACT_LIKE_SLASHNODE_MODULES_SLASH_TYPES),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "types",
        kind: CommandLineOptionKind::List,
        affects_program_structure: true,
        show_in_simplified_help_view: true,
        category: Some(&diagnostics::MODULES),
        description: Some(&diagnostics::SPECIFY_TYPE_PACKAGE_NAMES_TO_BE_INCLUDED_WITHOUT_BEING_REFERENCED_IN_A_SOURCE_FILE),
        transpile_option_value: Tristate::Unknown,
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "allowSyntheticDefaultImports",
        kind: CommandLineOptionKind::Boolean,
        affects_semantic_diagnostics: true,
        affects_build_info: true,
        category: Some(&diagnostics::INTEROP_CONSTRAINTS),
        description: Some(&diagnostics::ALLOW_IMPORT_X_FROM_Y_WHEN_A_MODULE_DOESN_T_HAVE_A_DEFAULT_EXPORT),
        default_value_description: DefaultValueDescription::Bool(true),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "esModuleInterop",
        kind: CommandLineOptionKind::Boolean,
        affects_semantic_diagnostics: true,
        affects_emit: true,
        affects_build_info: true,
        show_in_simplified_help_view: true,
        category: Some(&diagnostics::INTEROP_CONSTRAINTS),
        description: Some(&diagnostics::EMIT_ADDITIONAL_JAVA_SCRIPT_TO_EASE_SUPPORT_FOR_IMPORTING_COMMON_JS_MODULES_THIS_ENABLES_ALLOW_SYNTHETIC_DEFAULT_IMPORTS_FOR_TYPE_COMPATIBILITY),
        default_value_description: DefaultValueDescription::Bool(true),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "preserveSymlinks",
        kind: CommandLineOptionKind::Boolean,
        category: Some(&diagnostics::INTEROP_CONSTRAINTS),
        description: Some(&diagnostics::DISABLE_RESOLVING_SYMLINKS_TO_THEIR_REALPATH_THIS_CORRELATES_TO_THE_SAME_FLAG_IN_NODE),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "allowUmdGlobalAccess",
        kind: CommandLineOptionKind::Boolean,
        affects_semantic_diagnostics: true,
        affects_build_info: true,
        category: Some(&diagnostics::MODULES),
        description: Some(&diagnostics::ALLOW_ACCESSING_UMD_GLOBALS_FROM_MODULES),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "moduleSuffixes",
        kind: CommandLineOptionKind::List,
        list_preserve_falsy_values: true,
        affects_module_resolution: true,
        category: Some(&diagnostics::MODULES),
        description: Some(&diagnostics::LIST_OF_FILE_NAME_SUFFIXES_TO_SEARCH_WHEN_RESOLVING_A_MODULE),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "allowImportingTsExtensions",
        kind: CommandLineOptionKind::Boolean,
        affects_semantic_diagnostics: true,
        affects_build_info: true,
        category: Some(&diagnostics::MODULES),
        description: Some(&diagnostics::ALLOW_IMPORTS_TO_INCLUDE_TYPE_SCRIPT_FILE_EXTENSIONS_REQUIRES_MODULE_RESOLUTION_BUNDLER_AND_EITHER_NO_EMIT_OR_EMIT_DECLARATION_ONLY_TO_BE_SET),
        default_value_description: DefaultValueDescription::Bool(false),
        transpile_option_value: Tristate::Unknown,
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "rewriteRelativeImportExtensions",
        kind: CommandLineOptionKind::Boolean,
        affects_semantic_diagnostics: true,
        affects_build_info: true,
        category: Some(&diagnostics::MODULES),
        description: Some(&diagnostics::REWRITE_TS_TSX_MTS_AND_CTS_FILE_EXTENSIONS_IN_RELATIVE_IMPORT_PATHS_TO_THEIR_JAVA_SCRIPT_EQUIVALENT_IN_OUTPUT_FILES),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "resolvePackageJsonExports",
        kind: CommandLineOptionKind::Boolean,
        affects_module_resolution: true,
        category: Some(&diagnostics::MODULES),
        description: Some(&diagnostics::USE_THE_PACKAGE_JSON_EXPORTS_FIELD_WHEN_RESOLVING_PACKAGE_IMPORTS),
        default_value_description: DefaultValueDescription::Message(&diagnostics::X_TRUE_WHEN_MODULE_RESOLUTION_IS_NODE16_NODENEXT_OR_BUNDLER_OTHERWISE_FALSE),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "resolvePackageJsonImports",
        kind: CommandLineOptionKind::Boolean,
        affects_module_resolution: true,
        category: Some(&diagnostics::MODULES),
        description: Some(&diagnostics::USE_THE_PACKAGE_JSON_IMPORTS_FIELD_WHEN_RESOLVING_IMPORTS),
        default_value_description: DefaultValueDescription::Message(&diagnostics::X_TRUE_WHEN_MODULE_RESOLUTION_IS_NODE16_NODENEXT_OR_BUNDLER_OTHERWISE_FALSE),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "customConditions",
        kind: CommandLineOptionKind::List,
        affects_module_resolution: true,
        category: Some(&diagnostics::MODULES),
        description: Some(&diagnostics::CONDITIONS_TO_SET_IN_ADDITION_TO_THE_RESOLVER_SPECIFIC_DEFAULTS_WHEN_RESOLVING_IMPORTS),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "noUncheckedSideEffectImports",
        kind: CommandLineOptionKind::Boolean,
        affects_semantic_diagnostics: true,
        affects_build_info: true,
        category: Some(&diagnostics::MODULES),
        description: Some(&diagnostics::CHECK_SIDE_EFFECT_IMPORTS),
        default_value_description: DefaultValueDescription::Bool(true),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "sourceRoot",
        kind: CommandLineOptionKind::String,
        affects_emit: true,
        affects_build_info: true,
        category: Some(&diagnostics::EMIT),
        description: Some(&diagnostics::SPECIFY_THE_ROOT_PATH_FOR_DEBUGGERS_TO_FIND_THE_REFERENCE_SOURCE_CODE),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "mapRoot",
        kind: CommandLineOptionKind::String,
        affects_emit: true,
        affects_build_info: true,
        category: Some(&diagnostics::EMIT),
        description: Some(&diagnostics::SPECIFY_THE_LOCATION_WHERE_DEBUGGER_SHOULD_LOCATE_MAP_FILES_INSTEAD_OF_GENERATED_LOCATIONS),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "inlineSources",
        kind: CommandLineOptionKind::Boolean,
        affects_emit: true,
        affects_build_info: true,
        category: Some(&diagnostics::EMIT),
        description: Some(&diagnostics::INCLUDE_SOURCE_CODE_IN_THE_SOURCEMAPS_INSIDE_THE_EMITTED_JAVA_SCRIPT),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "experimentalDecorators",
        kind: CommandLineOptionKind::Boolean,
        affects_emit: true,
        affects_semantic_diagnostics: true,
        affects_build_info: true,
        category: Some(&diagnostics::LANGUAGE_AND_ENVIRONMENT),
        description: Some(&diagnostics::ENABLE_EXPERIMENTAL_SUPPORT_FOR_LEGACY_EXPERIMENTAL_DECORATORS),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "emitDecoratorMetadata",
        kind: CommandLineOptionKind::Boolean,
        affects_semantic_diagnostics: true,
        affects_emit: true,
        affects_build_info: true,
        category: Some(&diagnostics::LANGUAGE_AND_ENVIRONMENT),
        description: Some(&diagnostics::EMIT_DESIGN_TYPE_METADATA_FOR_DECORATED_DECLARATIONS_IN_SOURCE_FILES),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "jsxFactory",
        kind: CommandLineOptionKind::String,
        category: Some(&diagnostics::LANGUAGE_AND_ENVIRONMENT),
        description: Some(&diagnostics::SPECIFY_THE_JSX_FACTORY_FUNCTION_USED_WHEN_TARGETING_REACT_JSX_EMIT_E_G_REACT_CREATE_ELEMENT_OR_H),
        default_value_description: DefaultValueDescription::Str("`React.createElement`"),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "jsxFragmentFactory",
        kind: CommandLineOptionKind::String,
        category: Some(&diagnostics::LANGUAGE_AND_ENVIRONMENT),
        description: Some(&diagnostics::SPECIFY_THE_JSX_FRAGMENT_REFERENCE_USED_FOR_FRAGMENTS_WHEN_TARGETING_REACT_JSX_EMIT_E_G_REACT_FRAGMENT_OR_FRAGMENT),
        default_value_description: DefaultValueDescription::Str("React.Fragment"),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "jsxImportSource",
        kind: CommandLineOptionKind::String,
        affects_semantic_diagnostics: true,
        affects_emit: true,
        affects_build_info: true,
        affects_module_resolution: true,
        affects_source_file: true,
        category: Some(&diagnostics::LANGUAGE_AND_ENVIRONMENT),
        description: Some(&diagnostics::SPECIFY_MODULE_SPECIFIER_USED_TO_IMPORT_THE_JSX_FACTORY_FUNCTIONS_WHEN_USING_JSX_COLON_REACT_JSX_ASTERISK),
        default_value_description: DefaultValueDescription::Str("react"),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "resolveJsonModule",
        kind: CommandLineOptionKind::Boolean,
        affects_module_resolution: true,
        category: Some(&diagnostics::MODULES),
        description: Some(&diagnostics::ENABLE_IMPORTING_JSON_FILES),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "allowArbitraryExtensions",
        kind: CommandLineOptionKind::Boolean,
        affects_program_structure: true,
        category: Some(&diagnostics::MODULES),
        description: Some(&diagnostics::ENABLE_IMPORTING_FILES_WITH_ANY_EXTENSION_PROVIDED_A_DECLARATION_FILE_IS_PRESENT),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "reactNamespace",
        kind: CommandLineOptionKind::String,
        affects_emit: true,
        affects_build_info: true,
        category: Some(&diagnostics::LANGUAGE_AND_ENVIRONMENT),
        description: Some(&diagnostics::SPECIFY_THE_OBJECT_INVOKED_FOR_CREATE_ELEMENT_THIS_ONLY_APPLIES_WHEN_TARGETING_REACT_JSX_EMIT),
        default_value_description: DefaultValueDescription::Str("`React`"),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "skipDefaultLibCheck",
        kind: CommandLineOptionKind::Boolean,
        affects_build_info: true,
        category: Some(&diagnostics::COMPLETENESS),
        description: Some(&diagnostics::SKIP_TYPE_CHECKING_D_TS_FILES_THAT_ARE_INCLUDED_WITH_TYPE_SCRIPT),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "emitBOM",
        kind: CommandLineOptionKind::Boolean,
        affects_emit: true,
        affects_build_info: true,
        category: Some(&diagnostics::EMIT),
        description: Some(&diagnostics::EMIT_A_UTF_8_BYTE_ORDER_MARK_BOM_IN_THE_BEGINNING_OF_OUTPUT_FILES),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "newLine",
        kind: CommandLineOptionKind::Enum,
        affects_emit: true,
        affects_build_info: true,
        category: Some(&diagnostics::EMIT),
        description: Some(&diagnostics::SET_THE_NEWLINE_CHARACTER_FOR_EMITTING_FILES),
        default_value_description: DefaultValueDescription::Str("lf"),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "noErrorTruncation",
        kind: CommandLineOptionKind::Boolean,
        affects_semantic_diagnostics: true,
        affects_build_info: true,
        category: Some(&diagnostics::OUTPUT_FORMATTING),
        description: Some(&diagnostics::DISABLE_TRUNCATING_TYPES_IN_ERROR_MESSAGES),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "noLib",
        kind: CommandLineOptionKind::Boolean,
        category: Some(&diagnostics::LANGUAGE_AND_ENVIRONMENT),
        affects_program_structure: true,
        description: Some(&diagnostics::DISABLE_INCLUDING_ANY_LIBRARY_FILES_INCLUDING_THE_DEFAULT_LIB_D_TS),
        transpile_option_value: Tristate::True,
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "noResolve",
        kind: CommandLineOptionKind::Boolean,
        affects_module_resolution: true,
        category: Some(&diagnostics::MODULES),
        description: Some(&diagnostics::DISALLOW_IMPORT_S_REQUIRE_S_OR_REFERENCE_S_FROM_EXPANDING_THE_NUMBER_OF_FILES_TYPE_SCRIPT_SHOULD_ADD_TO_A_PROJECT),
        transpile_option_value: Tristate::True,
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "stripInternal",
        kind: CommandLineOptionKind::Boolean,
        affects_emit: true,
        affects_build_info: true,
        category: Some(&diagnostics::EMIT),
        description: Some(&diagnostics::DISABLE_EMITTING_DECLARATIONS_THAT_HAVE_INTERNAL_IN_THEIR_JSDOC_COMMENTS),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "disableSizeLimit",
        kind: CommandLineOptionKind::Boolean,
        affects_program_structure: true,
        category: Some(&diagnostics::EDITOR_SUPPORT),
        description: Some(&diagnostics::REMOVE_THE_20MB_CAP_ON_TOTAL_SOURCE_CODE_SIZE_FOR_JAVA_SCRIPT_FILES_IN_THE_TYPE_SCRIPT_LANGUAGE_SERVER),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "disableSourceOfProjectReferenceRedirect",
        kind: CommandLineOptionKind::Boolean,
        is_tsconfig_only: true,
        category: Some(&diagnostics::PROJECTS),
        description: Some(&diagnostics::DISABLE_PREFERRING_SOURCE_FILES_INSTEAD_OF_DECLARATION_FILES_WHEN_REFERENCING_COMPOSITE_PROJECTS),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "disableSolutionSearching",
        kind: CommandLineOptionKind::Boolean,
        is_tsconfig_only: true,
        category: Some(&diagnostics::PROJECTS),
        description: Some(&diagnostics::OPT_A_PROJECT_OUT_OF_MULTI_PROJECT_REFERENCE_CHECKING_WHEN_EDITING),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "disableReferencedProjectLoad",
        kind: CommandLineOptionKind::Boolean,
        is_tsconfig_only: true,
        category: Some(&diagnostics::PROJECTS),
        description: Some(&diagnostics::REDUCE_THE_NUMBER_OF_PROJECTS_LOADED_AUTOMATICALLY_BY_TYPE_SCRIPT),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "noEmitHelpers",
        kind: CommandLineOptionKind::Boolean,
        affects_emit: true,
        affects_build_info: true,
        category: Some(&diagnostics::EMIT),
        description: Some(&diagnostics::DISABLE_GENERATING_CUSTOM_HELPER_FUNCTIONS_LIKE_EXTENDS_IN_COMPILED_OUTPUT),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "noEmitOnError",
        kind: CommandLineOptionKind::Boolean,
        affects_emit: true,
        affects_build_info: true,
        category: Some(&diagnostics::EMIT),
        transpile_option_value: Tristate::Unknown,
        description: Some(&diagnostics::DISABLE_EMITTING_FILES_IF_ANY_TYPE_CHECKING_ERRORS_ARE_REPORTED),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "preserveConstEnums",
        kind: CommandLineOptionKind::Boolean,
        affects_emit: true,
        affects_build_info: true,
        category: Some(&diagnostics::EMIT),
        description: Some(&diagnostics::DISABLE_ERASING_CONST_ENUM_DECLARATIONS_IN_GENERATED_CODE),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "declarationDir",
        kind: CommandLineOptionKind::String,
        affects_emit: true,
        affects_build_info: true,
        affects_declaration_path: true,
        is_file_path: true,
        category: Some(&diagnostics::EMIT),
        transpile_option_value: Tristate::Unknown,
        description: Some(&diagnostics::SPECIFY_THE_OUTPUT_DIRECTORY_FOR_GENERATED_DECLARATION_FILES),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "skipLibCheck",
        kind: CommandLineOptionKind::Boolean,
        affects_build_info: true,
        category: Some(&diagnostics::COMPLETENESS),
        description: Some(&diagnostics::SKIP_TYPE_CHECKING_ALL_D_TS_FILES),
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "allowUnusedLabels",
        kind: CommandLineOptionKind::Boolean,
        affects_bind_diagnostics: true,
        affects_semantic_diagnostics: true,
        affects_build_info: true,
        category: Some(&diagnostics::TYPE_CHECKING),
        description: Some(&diagnostics::DISABLE_ERROR_REPORTING_FOR_UNUSED_LABELS),
        default_value_description: DefaultValueDescription::Tristate(Tristate::Unknown),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "allowUnreachableCode",
        kind: CommandLineOptionKind::Boolean,
        affects_bind_diagnostics: true,
        affects_semantic_diagnostics: true,
        affects_build_info: true,
        category: Some(&diagnostics::TYPE_CHECKING),
        description: Some(&diagnostics::DISABLE_ERROR_REPORTING_FOR_UNREACHABLE_CODE),
        default_value_description: DefaultValueDescription::Tristate(Tristate::Unknown),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "forceConsistentCasingInFileNames",
        kind: CommandLineOptionKind::Boolean,
        affects_module_resolution: true,
        category: Some(&diagnostics::INTEROP_CONSTRAINTS),
        description: Some(&diagnostics::ENSURE_THAT_CASING_IS_CORRECT_IN_IMPORTS),
        default_value_description: DefaultValueDescription::Bool(true),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "maxNodeModuleJsDepth",
        kind: CommandLineOptionKind::Number,
        affects_module_resolution: true,
        category: Some(&diagnostics::JAVA_SCRIPT_SUPPORT),
        description: Some(&diagnostics::SPECIFY_THE_MAXIMUM_FOLDER_DEPTH_USED_FOR_CHECKING_JAVA_SCRIPT_FILES_FROM_NODE_MODULES_ONLY_APPLICABLE_WITH_ALLOW_JS),
        default_value_description: DefaultValueDescription::Int(0),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "useDefineForClassFields",
        kind: CommandLineOptionKind::Boolean,
        affects_semantic_diagnostics: true,
        affects_emit: true,
        affects_build_info: true,
        category: Some(&diagnostics::LANGUAGE_AND_ENVIRONMENT),
        description: Some(&diagnostics::EMIT_ECMA_SCRIPT_STANDARD_COMPLIANT_CLASS_FIELDS),
        default_value_description: DefaultValueDescription::Message(&diagnostics::X_TRUE_FOR_ES2022_AND_ABOVE_INCLUDING_ES_NEXT),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "plugins",
        kind: CommandLineOptionKind::List,
        is_tsconfig_only: true,
        description: Some(&diagnostics::SPECIFY_A_LIST_OF_LANGUAGE_SERVICE_PLUGINS_TO_INCLUDE),
        category: Some(&diagnostics::EDITOR_SUPPORT),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "moduleDetection",
        kind: CommandLineOptionKind::Enum,
        affects_source_file: true,
        affects_module_resolution: true,
        description: Some(&diagnostics::CONTROL_WHAT_METHOD_IS_USED_TO_DETECT_MODULE_FORMAT_JS_FILES),
        category: Some(&diagnostics::LANGUAGE_AND_ENVIRONMENT),
        default_value_description: DefaultValueDescription::Message(&diagnostics::X_AUTO_COLON_TREAT_FILES_WITH_IMPORTS_EXPORTS_IMPORT_META_JSX_WITH_JSX_COLON_REACT_JSX_OR_ESM_FORMAT_WITH_MODULE_COLON_NODE16_AS_MODULES),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "ignoreDeprecations",
        kind: CommandLineOptionKind::String,
        default_value_description: DefaultValueDescription::Tristate(Tristate::Unknown),
        ..CommandLineOption::DEFAULT
    },
];

pub(crate) static OPTIONS_FOR_COMPILER: LazyLock<Vec<&'static CommandLineOption>> =
    LazyLock::new(|| {
        vec![
            &OPTIONS_FOR_COMPILER_ITEMS[0],
            &OPTIONS_FOR_COMPILER_ITEMS[1],
            &OPTIONS_FOR_COMPILER_ITEMS[2],
            &OPTIONS_FOR_COMPILER_ITEMS[3],
            &OPTIONS_FOR_COMPILER_ITEMS[4],
            &OPTIONS_FOR_COMPILER_ITEMS[5],
            &OPTIONS_FOR_COMPILER_ITEMS[6],
            &OPTIONS_FOR_COMPILER_ITEMS[7],
            &OPTIONS_FOR_COMPILER_ITEMS[8],
            &OPTIONS_FOR_COMPILER_ITEMS[9],
            &OPTIONS_FOR_COMPILER_ITEMS[10],
            &OPTIONS_FOR_COMPILER_ITEMS[11],
            &OPTIONS_FOR_COMPILER_ITEMS[12],
            &OPTIONS_FOR_COMPILER_ITEMS[13],
            &OPTIONS_FOR_COMPILER_ITEMS[14],
            &OPTIONS_FOR_COMPILER_ITEMS[15],
            &OPTIONS_FOR_COMPILER_ITEMS[16],
            &OPTIONS_FOR_COMPILER_ITEMS[17],
            &OPTIONS_FOR_COMPILER_ITEMS[18],
            &OPTIONS_FOR_COMPILER_ITEMS[19],
            &OPTIONS_FOR_COMPILER_ITEMS[20],
            &OPTIONS_FOR_COMPILER_ITEMS[21],
            &OPTIONS_FOR_COMPILER_ITEMS[22],
            &OPTIONS_FOR_COMPILER_ITEMS[23],
            &OPTIONS_FOR_COMPILER_ITEMS[24],
            &OPTIONS_FOR_COMPILER_ITEMS[25],
            &OPTIONS_FOR_COMPILER_ITEMS[26],
            &OPTIONS_FOR_COMPILER_ITEMS[27],
            &OPTIONS_FOR_COMPILER_ITEMS[28],
            &OPTIONS_FOR_COMPILER_ITEMS[29],
            &OPTIONS_FOR_COMPILER_ITEMS[30],
            &OPTIONS_FOR_COMPILER_ITEMS[31],
            &OPTIONS_FOR_COMPILER_ITEMS[32],
            &OPTIONS_FOR_COMPILER_ITEMS[33],
            &OPTIONS_FOR_COMPILER_ITEMS[34],
            &OPTIONS_FOR_COMPILER_ITEMS[35],
            &OPTIONS_FOR_COMPILER_ITEMS[36],
            &OPTIONS_FOR_COMPILER_ITEMS[37],
            &OPTIONS_FOR_COMPILER_ITEMS[38],
            &OPTIONS_FOR_COMPILER_ITEMS[39],
            &OPTIONS_FOR_COMPILER_ITEMS[40],
            &OPTIONS_FOR_COMPILER_ITEMS[41],
            &OPTIONS_FOR_COMPILER_ITEMS[42],
            &OPTIONS_FOR_COMPILER_ITEMS[43],
            &OPTIONS_FOR_COMPILER_ITEMS[44],
            &OPTIONS_FOR_COMPILER_ITEMS[45],
            &OPTIONS_FOR_COMPILER_ITEMS[46],
            &OPTIONS_FOR_COMPILER_ITEMS[47],
            &OPTIONS_FOR_COMPILER_ITEMS[48],
            &OPTIONS_FOR_COMPILER_ITEMS[49],
            &OPTIONS_FOR_COMPILER_ITEMS[50],
            &OPTIONS_FOR_COMPILER_ITEMS[51],
            &OPTIONS_FOR_COMPILER_ITEMS[52],
            &OPTIONS_FOR_COMPILER_ITEMS[53],
            &OPTIONS_FOR_COMPILER_ITEMS[54],
            &OPTIONS_FOR_COMPILER_ITEMS[55],
            &OPTIONS_FOR_COMPILER_ITEMS[56],
            &OPTIONS_FOR_COMPILER_ITEMS[57],
            &OPTIONS_FOR_COMPILER_ITEMS[58],
            &OPTIONS_FOR_COMPILER_ITEMS[59],
            &OPTIONS_FOR_COMPILER_ITEMS[60],
            &OPTIONS_FOR_COMPILER_ITEMS[61],
            &OPTIONS_FOR_COMPILER_ITEMS[62],
            &OPTIONS_FOR_COMPILER_ITEMS[63],
            &OPTIONS_FOR_COMPILER_ITEMS[64],
            &OPTIONS_FOR_COMPILER_ITEMS[65],
            &OPTIONS_FOR_COMPILER_ITEMS[66],
            &OPTIONS_FOR_COMPILER_ITEMS[67],
            &OPTIONS_FOR_COMPILER_ITEMS[68],
            &OPTIONS_FOR_COMPILER_ITEMS[69],
            &OPTIONS_FOR_COMPILER_ITEMS[70],
            &OPTIONS_FOR_COMPILER_ITEMS[71],
            &OPTIONS_FOR_COMPILER_ITEMS[72],
            &OPTIONS_FOR_COMPILER_ITEMS[73],
            &OPTIONS_FOR_COMPILER_ITEMS[74],
            &OPTIONS_FOR_COMPILER_ITEMS[75],
            &OPTIONS_FOR_COMPILER_ITEMS[76],
            &OPTIONS_FOR_COMPILER_ITEMS[77],
            &OPTIONS_FOR_COMPILER_ITEMS[78],
            &OPTIONS_FOR_COMPILER_ITEMS[79],
            &OPTIONS_FOR_COMPILER_ITEMS[80],
            &OPTIONS_FOR_COMPILER_ITEMS[81],
            &OPTIONS_FOR_COMPILER_ITEMS[82],
            &OPTIONS_FOR_COMPILER_ITEMS[83],
            &OPTIONS_FOR_COMPILER_ITEMS[84],
            &OPTIONS_FOR_COMPILER_ITEMS[85],
            &OPTIONS_FOR_COMPILER_ITEMS[86],
            &OPTIONS_FOR_COMPILER_ITEMS[87],
            &OPTIONS_FOR_COMPILER_ITEMS[88],
            &OPTIONS_FOR_COMPILER_ITEMS[89],
            &OPTIONS_FOR_COMPILER_ITEMS[90],
            &OPTIONS_FOR_COMPILER_ITEMS[91],
            &OPTIONS_FOR_COMPILER_ITEMS[92],
            &OPTIONS_FOR_COMPILER_ITEMS[93],
            &OPTIONS_FOR_COMPILER_ITEMS[94],
            &OPTIONS_FOR_COMPILER_ITEMS[95],
            &OPTIONS_FOR_COMPILER_ITEMS[96],
        ]
    });
// END GENERATED

#[derive(Clone, Copy, Default, Eq, PartialEq)]
struct CompilerOptionChanges(u8);

impl CompilerOptionChanges {
    const SEMANTIC_DIAGNOSTICS: Self = Self(1 << 0);
    const DECLARATION_PATH: Self = Self(1 << 1);
    const EMIT: Self = Self(1 << 2);
    const ALL: Self = Self(Self::SEMANTIC_DIAGNOSTICS.0 | Self::DECLARATION_PATH.0 | Self::EMIT.0);

    fn from_declaration(option: &CommandLineOption) -> Self {
        Self(
            (option.affects_semantic_diagnostics as u8)
                | ((option.affects_declaration_path as u8) << 1)
                | ((option.affects_emit as u8) << 2),
        )
    }

    fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }

    fn contains(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }
}

#[inline(never)]
fn compiler_option_changes(
    old_options: Option<&CompilerOptions>,
    new_options: Option<&CompilerOptions>,
) -> CompilerOptionChanges {
    let (old_options, new_options) = match (old_options, new_options) {
        (None, None) => return CompilerOptionChanges::default(),
        (Some(old), Some(new)) if std::ptr::eq(old, new) => {
            return CompilerOptionChanges::default();
        }
        (Some(old), Some(new)) => (old, new),
        _ => return CompilerOptionChanges::ALL,
    };
    let mut changes = CompilerOptionChanges::default();
    // Go ForEachCompilerOptionValue walks the struct fields by reflection and looks each one up by name.
    macro_rules! compare_fields {
        ($($field:ident: $json:literal,)*) => {
            $(
                if let Some(option_declaration) = COMMAND_LINE_COMPILER_OPTIONS_MAP.get($json) {
                    let affected = CompilerOptionChanges::from_declaration(option_declaration);
                    if affected != CompilerOptionChanges::default() {
                        let changed = if option_declaration.strict_flag {
                            compare_strict(old_options, new_options, &old_options.$field, &new_options.$field)
                        } else if option_declaration.allow_js_flag {
                            old_options.get_allow_js() != new_options.get_allow_js()
                        } else {
                            old_options.$field != new_options.$field
                        };
                        if changed {
                            changes.insert(affected);
                            if changes == CompilerOptionChanges::ALL {
                                return changes;
                            }
                        }
                    }
                }
            )*
        };
    }
    for_each_compiler_options_field!(compare_fields);
    changes
}

fn compare_strict<T: std::any::Any>(
    old_options: &CompilerOptions,
    new_options: &CompilerOptions,
    old_value: &T,
    new_value: &T,
) -> bool {
    let old_value = (old_value as &dyn std::any::Any).downcast_ref::<Tristate>().copied().unwrap();
    let new_value = (new_value as &dyn std::any::Any).downcast_ref::<Tristate>().copied().unwrap();
    old_options.get_strict_option_value(old_value) != new_options.get_strict_option_value(new_value)
}

pub fn compiler_options_affect_semantic_diagnostics(
    old_options: Option<&CompilerOptions>,
    new_options: Option<&CompilerOptions>,
) -> bool {
    compiler_option_changes(old_options, new_options)
        .contains(CompilerOptionChanges::SEMANTIC_DIAGNOSTICS)
}

pub fn compiler_options_affect_declaration_path(
    old_options: Option<&CompilerOptions>,
    new_options: Option<&CompilerOptions>,
) -> bool {
    compiler_option_changes(old_options, new_options)
        .contains(CompilerOptionChanges::DECLARATION_PATH)
}

pub fn compiler_options_affect_emit(
    old_options: Option<&CompilerOptions>,
    new_options: Option<&CompilerOptions>,
) -> bool {
    compiler_option_changes(old_options, new_options).contains(CompilerOptionChanges::EMIT)
}
