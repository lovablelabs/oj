// Port of execute/build/compilerHost.go.

use std::sync::Arc;

use tsrs_ast::{SourceFile, SourceFileParseOptions};
use tsrs_compiler::CompilerHost;
use tsrs_contentmapper::{self as contentmapper, Mapper, Project};
use tsrs_core::P;
use tsrs_core::tspath::Path;
use tsrs_diagnostics::Message;
use tsrs_tsoptions::ParsedCommandLine;
use tsrs_vfs::FS;

use super::host::HostImpl;

pub struct CompilerHostImpl {
    pub(crate) host: &'static HostImpl,
    pub(crate) trace: Box<dyn Fn(&'static Message, &[&dyn std::fmt::Display]) + Send + Sync>,
    pub(crate) content_mapper_project: Option<Arc<dyn Project>>,
}

impl CompilerHost for CompilerHostImpl {
    fn fs(&self) -> &dyn FS {
        CompilerHost::fs(self.host)
    }

    fn default_library_path(&self) -> &str {
        self.host.default_library_path()
    }

    fn get_current_directory(&self) -> &str {
        CompilerHost::get_current_directory(self.host)
    }

    fn trace(&self, msg: &'static Message, args: &[&dyn std::fmt::Display]) {
        (self.trace)(msg, args);
    }

    fn get_source_file(&self, opts: SourceFileParseOptions) -> Option<P<SourceFile>> {
        self.host.get_source_file(opts)
    }

    // compilerHost.go:41
    fn get_content_mapped_source_files(
        &self,
        parse_options: SourceFileParseOptions,
        mapper: &Mapper,
    ) -> Result<contentmapper::SourceFiles, contentmapper::Error> {
        tsrs_compiler::get_content_mapped_source_files_with(
            self.fs(),
            self.content_mapper_project.as_deref(),
            parse_options,
            mapper,
        )
    }

    // compilerHost.go:56
    fn content_mapper_project(&self) -> Option<Arc<dyn Project>> {
        self.content_mapper_project.clone()
    }

    fn get_resolved_project_reference(
        &self,
        file_name: &str,
        path: Path,
    ) -> Option<P<ParsedCommandLine>> {
        self.host.get_resolved_project_reference(file_name, path)
    }
}
