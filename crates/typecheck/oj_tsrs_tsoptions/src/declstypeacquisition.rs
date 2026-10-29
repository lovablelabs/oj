use std::sync::LazyLock;

use crate::commandlineoption::{CommandLineOption, CommandLineOptionKind, DefaultValueDescription};
use crate::tsconfigparsing::command_line_options_to_map;

pub static TYPE_ACQUISITION_DECLARATION: LazyLock<CommandLineOption> =
    LazyLock::new(|| CommandLineOption {
        name: "typeAcquisition",
        kind: CommandLineOptionKind::Object,
        element_options: Some(command_line_options_to_map(&TYPE_ACQUISITION_DECLS)),
        ..CommandLineOption::DEFAULT
    });

// Do not delete this without updating the website's tsconfig generation.
pub static TYPE_ACQUISITION_DECLS_ITEMS: [CommandLineOption; 4] = [
    CommandLineOption {
        name: "enable",
        kind: CommandLineOptionKind::Boolean,
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "include",
        kind: CommandLineOptionKind::List,
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "exclude",
        kind: CommandLineOptionKind::List,
        ..CommandLineOption::DEFAULT
    },
    CommandLineOption {
        name: "disableFilenameBasedTypeAcquisition",
        kind: CommandLineOptionKind::Boolean,
        default_value_description: DefaultValueDescription::Bool(false),
        ..CommandLineOption::DEFAULT
    },
];

pub static TYPE_ACQUISITION_DECLS: LazyLock<Vec<&'static CommandLineOption>> =
    LazyLock::new(|| TYPE_ACQUISITION_DECLS_ITEMS.iter().collect());
