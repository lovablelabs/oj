use std::sync::LazyLock;

use tsrs_diagnostics as diagnostics;
use tsrs_diagnostics::Message;

use crate::commandlineoption::CommandLineOption;
use crate::declsbuild::BUILD_OPTS;
use crate::declscompiler::OPTIONS_DECLARATIONS;
use crate::namemap::{BUILD_NAME_MAP, COMPILER_NAME_MAP, NameMap};

pub struct DidYouMeanOptionsDiagnostics {
    pub(crate) alternate_mode: Option<AlternateModeDiagnostics>,
    pub(crate) option_declarations: Vec<&'static CommandLineOption>,
    pub(crate) unknown_option_diagnostic: &'static Message,
    pub(crate) unknown_did_you_mean_diagnostic: &'static Message,
}

pub struct AlternateModeDiagnostics {
    pub(crate) diagnostic: &'static Message,
    pub(crate) options_name_map: Option<&'static NameMap>,
}

pub struct ParseCommandLineWorkerDiagnostics {
    pub(crate) did_you_mean: DidYouMeanOptionsDiagnostics,
    pub(crate) option_type_mismatch_diagnostic: &'static Message,
}

pub static COMPILER_OPTIONS_DID_YOU_MEAN_DIAGNOSTICS: LazyLock<ParseCommandLineWorkerDiagnostics> =
    LazyLock::new(|| get_parse_command_line_worker_diagnostics(&OPTIONS_DECLARATIONS));

pub fn get_parse_command_line_worker_diagnostics(
    decls: &[&'static CommandLineOption],
) -> ParseCommandLineWorkerDiagnostics {
    // this will only return the correct diagnostics for `compiler` mode, and is factored into a function for testing reasons.
    ParseCommandLineWorkerDiagnostics {
        did_you_mean: DidYouMeanOptionsDiagnostics {
            alternate_mode: Some(AlternateModeDiagnostics {
                diagnostic: &diagnostics::COMPILER_OPTION_0_MAY_ONLY_BE_USED_WITH_BUILD,
                options_name_map: Some(&BUILD_NAME_MAP),
            }),
            option_declarations: decls.to_vec(),
            unknown_option_diagnostic: &diagnostics::UNKNOWN_COMPILER_OPTION_0,
            unknown_did_you_mean_diagnostic: &diagnostics::UNKNOWN_COMPILER_OPTION_0_DID_YOU_MEAN_1,
        },
        option_type_mismatch_diagnostic: &diagnostics::COMPILER_OPTION_0_EXPECTS_AN_ARGUMENT,
    }
}

pub(crate) static BUILD_OPTIONS_DID_YOU_MEAN_DIAGNOSTICS: LazyLock<
    ParseCommandLineWorkerDiagnostics,
> = LazyLock::new(|| ParseCommandLineWorkerDiagnostics {
    did_you_mean: DidYouMeanOptionsDiagnostics {
        alternate_mode: Some(AlternateModeDiagnostics {
            diagnostic: &diagnostics::COMPILER_OPTION_0_MAY_NOT_BE_USED_WITH_BUILD,
            options_name_map: Some(&COMPILER_NAME_MAP),
        }),
        option_declarations: BUILD_OPTS.clone(),
        unknown_option_diagnostic: &diagnostics::UNKNOWN_BUILD_OPTION_0,
        unknown_did_you_mean_diagnostic: &diagnostics::UNKNOWN_BUILD_OPTION_0_DID_YOU_MEAN_1,
    },
    option_type_mismatch_diagnostic: &diagnostics::BUILD_OPTION_0_REQUIRES_A_VALUE_OF_TYPE_1,
});
