use std::sync::LazyLock;

use tsrs_diagnostics as diagnostics;

use crate::commandlineoption::{CommandLineOption, CommandLineOptionKind, DefaultValueDescription};

use crate::declscompiler::COMMON_OPTIONS_WITH_BUILD;

// BEGIN GENERATED
pub(crate) static TSC_BUILD_OPTION: CommandLineOption = CommandLineOption {
    name: "build",
    kind: CommandLineOptionKind::Boolean,
    short_name: "b",
    show_in_simplified_help_view: true,
    category: Some(&diagnostics::COMMAND_LINE_OPTIONS),
    description: Some(
        &diagnostics::BUILD_ONE_OR_MORE_PROJECTS_AND_THEIR_DEPENDENCIES_IF_OUT_OF_DATE,
    ),
    default_value_description: DefaultValueDescription::Bool(false),
    ..CommandLineOption::DEFAULT
};

static OPTIONS_FOR_BUILD_ITEMS: [CommandLineOption; 6] = [
    CommandLineOption {
        name: "verbose",
        short_name: "v",
        category: Some(&diagnostics::COMMAND_LINE_OPTIONS),
        description: Some(&diagnostics::ENABLE_VERBOSE_LOGGING),
        kind: CommandLineOptionKind::Boolean,
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "dry",
        short_name: "d",
        category: Some(&diagnostics::COMMAND_LINE_OPTIONS),
        description: Some(
            &diagnostics::SHOW_WHAT_WOULD_BE_BUILT_OR_DELETED_IF_SPECIFIED_WITH_CLEAN,
        ),
        kind: CommandLineOptionKind::Boolean,
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "force",
        short_name: "f",
        category: Some(&diagnostics::COMMAND_LINE_OPTIONS),
        description: Some(
            &diagnostics::BUILD_ALL_PROJECTS_INCLUDING_THOSE_THAT_APPEAR_TO_BE_UP_TO_DATE,
        ),
        kind: CommandLineOptionKind::Boolean,
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "clean",
        category: Some(&diagnostics::COMMAND_LINE_OPTIONS),
        description: Some(&diagnostics::DELETE_THE_OUTPUTS_OF_ALL_PROJECTS),
        kind: CommandLineOptionKind::Boolean,
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "builders",
        kind: CommandLineOptionKind::Number,
        category: Some(&diagnostics::COMMAND_LINE_OPTIONS),
        description: Some(&diagnostics::SET_THE_NUMBER_OF_PROJECTS_TO_BUILD_CONCURRENTLY),
        default_value_description: DefaultValueDescription::Message(
            &diagnostics::X_4_UNLESS_SINGLE_THREADED_IS_PASSED,
        ),
        min_value: 1,
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "stopBuildOnErrors",
        category: Some(&diagnostics::COMMAND_LINE_OPTIONS),
        description: Some(
            &diagnostics::SKIP_BUILDING_DOWNSTREAM_PROJECTS_ON_ERROR_IN_UPSTREAM_PROJECT,
        ),
        kind: CommandLineOptionKind::Boolean,
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
];

static OPTIONS_FOR_BUILD: LazyLock<Vec<&'static CommandLineOption>> = LazyLock::new(|| {
    vec![
        &TSC_BUILD_OPTION,
        &OPTIONS_FOR_BUILD_ITEMS[0],
        &OPTIONS_FOR_BUILD_ITEMS[1],
        &OPTIONS_FOR_BUILD_ITEMS[2],
        &OPTIONS_FOR_BUILD_ITEMS[3],
        &OPTIONS_FOR_BUILD_ITEMS[4],
        &OPTIONS_FOR_BUILD_ITEMS[5],
    ]
});
// END GENERATED

pub(crate) static BUILD_OPTS: LazyLock<Vec<&'static CommandLineOption>> = LazyLock::new(|| {
    COMMON_OPTIONS_WITH_BUILD.iter().chain(OPTIONS_FOR_BUILD.iter()).copied().collect()
});
