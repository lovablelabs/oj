// Port of execute/incremental/incremental.go.

use std::sync::Arc;

use tsrs_compiler::CompilerHost;
use tsrs_core::P;
use tsrs_tsoptions::ParsedCommandLine;

use crate::buildinfo::{BuildInfo, content_mapper_identities};
use crate::buildinfotosnapshot::build_info_to_snapshot;
use crate::program::Program;

pub trait BuildInfoReader {
    fn read_build_info(&self, config: &ParsedCommandLine) -> Option<BuildInfo>;
}

struct BuildInfoReaderImpl {
    host: Arc<dyn CompilerHost>,
    canonical_diagnostics: bool,
}

impl BuildInfoReader for BuildInfoReaderImpl {
    // incremental.go:19
    fn read_build_info(&self, config: &ParsedCommandLine) -> Option<BuildInfo> {
        let build_info_file_name = config.get_build_info_file_name();
        if build_info_file_name.is_empty() {
            return None;
        }

        // Read build info file
        let data = tsrs_core::phases::time(
            config.compiler_options().unwrap().diagnostics_enabled(),
            "BuildInfo read: read file",
            || self.host.fs().read_file(&build_info_file_name),
        )?;
        tsrs_core::phases::time(
            config.compiler_options().unwrap().diagnostics_enabled(),
            "BuildInfo read: unmarshal",
            || {
                BuildInfo::unmarshal_with_canonical_diagnostics(&data, self.canonical_diagnostics)
                    .ok()
            },
        )
    }
}

/// Adds the `BuildInfo read: ...` phase rows (at zero) so that they print in the same place when the read runs
/// concurrently with program construction.
pub fn register_build_info_read_phases() {
    for name in
        ["BuildInfo read: read file", "BuildInfo read: unmarshal", "BuildInfo read: to snapshot"]
    {
        tsrs_core::phases::record(name, std::time::Duration::ZERO);
    }
}

pub fn new_build_info_reader(host: Arc<dyn CompilerHost>) -> Box<dyn BuildInfoReader> {
    new_build_info_reader_with_canonical_diagnostics(host, true)
}

pub fn new_build_info_reader_with_canonical_diagnostics(
    host: Arc<dyn CompilerHost>,
    canonical_diagnostics: bool,
) -> Box<dyn BuildInfoReader> {
    Box::new(BuildInfoReaderImpl { host, canonical_diagnostics })
}

// incremental.go:44
pub fn read_build_info_program(
    config: P<ParsedCommandLine>,
    reader: &dyn BuildInfoReader,
    host: &dyn CompilerHost,
) -> Option<P<Program>> {
    // Read buildInfo file
    let build_info = reader.read_build_info(&config)?;
    if !build_info.is_valid_version() || !build_info.is_incremental() {
        return None;
    }
    // If any configured content mapper's identity has changed, files it produced may be stale, so the
    // old program cannot be reused.
    match content_mapper_identities(host.content_mapper_project().as_deref()) {
        Ok(identities) if build_info.content_mapper_identities_match(identities.as_deref()) => {}
        _ => return None,
    }

    // Convert to information that can be used to create incremental program
    Some(Program::new_from_snapshot(tsrs_core::phases::time(
        config.compiler_options().unwrap().diagnostics_enabled(),
        "BuildInfo read: to snapshot",
        || build_info_to_snapshot(&build_info, &config, host),
    )))
}
