use std::fmt::Display;
use std::sync::Arc;

use tsrs_ast::{SourceFile, SourceFileParseOptions};
use tsrs_contentmapper::{self as contentmapper, Mapper, Project};
use tsrs_core::tspath::Path;
use tsrs_core::{P, ensure_script_kind_from_file_name};
use tsrs_diagnostics::Message;
use tsrs_tsoptions::{ExtendedConfigCache, ParseConfigHost, ParsedCommandLine};
use tsrs_vfs::FS;

pub type TraceFn = dyn Fn(&'static Message, &[&dyn Display]) + Send + Sync;

// host.go:15
pub trait CompilerHost: Send + Sync {
    fn fs(&self) -> &dyn FS;
    fn default_library_path(&self) -> &str;
    fn get_current_directory(&self) -> &str;
    fn trace(&self, msg: &'static Message, args: &[&dyn Display]);
    fn get_source_file(&self, opts: SourceFileParseOptions) -> Option<P<SourceFile>>;
    // GetContentMappedSourceFile produces the source file for a content-mapped (foreign) file by running
    // the given mapper's transform on the file's content. The caller resolves the mapper (and owns the
    // failure accounting), so implementations must use it as-is. It returns nil if the file cannot be read,
    // or an error if the transform fails or the mapper produces invalid position mappings. Implementations
    // may cache successful results.
    // (A file that cannot be read is `Ok` with no canonical file, Go's zero `SourceFiles` and nil error.)
    fn get_content_mapped_source_files(
        &self,
        parse_options: SourceFileParseOptions,
        mapper: &Mapper,
    ) -> Result<contentmapper::SourceFiles, contentmapper::Error>;
    // ContentMapperProject returns the project-scoped content mapper used by this host, or nil when the
    // command line has no content mappers. The project owns transform identity and lifecycle state.
    fn content_mapper_project(&self) -> Option<Arc<dyn Project>>;
    fn get_resolved_project_reference(
        &self,
        file_name: &str,
        path: Path,
    ) -> Option<P<ParsedCommandLine>>;
}

// host.go:35
pub struct CompilerHostImpl {
    current_directory: String,
    fs: Arc<dyn FS>,
    default_library_path: String,
    extended_config_cache: Option<Arc<dyn ExtendedConfigCache + Send>>,
    trace: Box<TraceFn>,
    content_mapper_project: Option<Arc<dyn Project>>,
}

// host.go:44
pub fn new_cached_fs_compiler_host(
    current_directory: &str,
    fs: Arc<dyn FS>,
    default_library_path: &str,
    extended_config_cache: Option<Arc<dyn ExtendedConfigCache + Send>>,
    trace: Option<Box<TraceFn>>,
    content_mapper_project: Option<Arc<dyn Project>>,
) -> Arc<dyn CompilerHost> {
    new_compiler_host(
        current_directory,
        Arc::new(tsrs_vfs::cachedvfs::from(fs)),
        default_library_path,
        extended_config_cache,
        trace,
        content_mapper_project,
    )
}

// host.go:55
pub fn new_compiler_host(
    current_directory: &str,
    fs: Arc<dyn FS>,
    default_library_path: &str,
    extended_config_cache: Option<Arc<dyn ExtendedConfigCache + Send>>,
    trace: Option<Box<TraceFn>>,
    content_mapper_project: Option<Arc<dyn Project>>,
) -> Arc<dyn CompilerHost> {
    let trace = trace.unwrap_or_else(|| Box::new(|_: &'static Message, _: &[&dyn Display]| {}));
    Arc::new(CompilerHostImpl {
        current_directory: current_directory.to_string(),
        fs,
        default_library_path: default_library_path.to_string(),
        extended_config_cache,
        trace,
        content_mapper_project,
    })
}

// host.go:101. Shared with the build orchestrator's per-project compiler host (build/compilerHost.go:41), which
// has the same body.
pub fn get_content_mapped_source_files_with(
    fs: &dyn FS,
    content_mapper_project: Option<&dyn Project>,
    parse_options: SourceFileParseOptions,
    mapper: &Mapper,
) -> Result<contentmapper::SourceFiles, contentmapper::Error> {
    let Some(content_mapper_project) = content_mapper_project else {
        return Err(contentmapper::Error::ProjectUnavailable);
    };
    let Some(content) = fs.read_file(&parse_options.file_name) else {
        return Ok(contentmapper::SourceFiles::default());
    };
    let files = contentmapper::transform_and_parse(
        parse_options,
        &content,
        mapper,
        content_mapper_project,
    )?;
    contentmapper::check_supplemental_file_name_collisions(&files, |file_name| {
        fs.file_exists(file_name)
    })?;
    Ok(files)
}

impl CompilerHost for CompilerHostImpl {
    fn fs(&self) -> &dyn FS {
        &*self.fs
    }

    fn default_library_path(&self) -> &str {
        &self.default_library_path
    }

    fn get_current_directory(&self) -> &str {
        &self.current_directory
    }

    fn trace(&self, msg: &'static Message, args: &[&dyn Display]) {
        (self.trace)(msg, args);
    }

    fn get_source_file(&self, opts: SourceFileParseOptions) -> Option<P<SourceFile>> {
        let script_kind = ensure_script_kind_from_file_name(&opts.file_name);
        // tsrs-only: a bundled lib (`bundled:///libs/lib.*.d.ts`) is text in the binary; parse it in place instead of
        // reading a heap copy and leaking it (about 2.9 MB for a typical lib set). Same
        // bytes, same `&'static str` the identifiers point into, so nothing downstream changes.
        if let Some(text) = tsrs_vfs::bundled::embedded_file(&opts.file_name) {
            return Some(tsrs_core::festats::timed(tsrs_core::festats::Cat::Parse, || {
                tsrs_parser::parse_source_file_embedded(opts, text, script_kind)
            }));
        }
        let text = tsrs_core::festats::timed(tsrs_core::festats::Cat::Read, || {
            self.fs().read_file(&opts.file_name)
        })?;
        // tsrs-only: the CLI's `--noEmit` check parses each file that may be a leaf into a region of its own, freed
        // once the file is checked if it is one (fileregions.rs).
        if crate::fileregions::wants_region(&opts.file_name, script_kind) {
            return Some(tsrs_core::festats::timed(tsrs_core::festats::Cat::Parse, || {
                crate::fileregions::parse(opts, text, script_kind)
            }));
        }
        Some(tsrs_core::festats::timed(tsrs_core::festats::Cat::Parse, || {
            tsrs_parser::parse_source_file_owned(opts, text, script_kind)
        }))
    }

    // host.go:101
    fn get_content_mapped_source_files(
        &self,
        parse_options: SourceFileParseOptions,
        mapper: &Mapper,
    ) -> Result<contentmapper::SourceFiles, contentmapper::Error> {
        get_content_mapped_source_files_with(
            self.fs(),
            self.content_mapper_project.as_deref(),
            parse_options,
            mapper,
        )
    }

    // host.go:115
    fn content_mapper_project(&self) -> Option<Arc<dyn Project>> {
        self.content_mapper_project.clone()
    }

    // host.go:119
    fn get_resolved_project_reference(
        &self,
        file_name: &str,
        path: Path,
    ) -> Option<P<ParsedCommandLine>> {
        // Go passes the host itself as the `ParseConfigHost`; this one borrows the same file system and directory for
        // the parse call.
        let sys = ParseConfigHostImpl {
            fs: Arc::clone(&self.fs),
            current_directory: self.current_directory.clone(),
        };
        let (command_line, _) = tsrs_tsoptions::get_parsed_command_line_of_config_file_path(
            file_name,
            path,
            None,
            None, /*optionsRaw*/
            &sys,
            self.extended_config_cache.as_deref().map(|c| c as &dyn ExtendedConfigCache),
        );
        command_line.map(P::new)
    }
}

struct ParseConfigHostImpl {
    fs: Arc<dyn FS>,
    current_directory: String,
}

impl ParseConfigHost for ParseConfigHostImpl {
    fn fs(&self) -> &dyn FS {
        &*self.fs
    }

    fn get_current_directory(&self) -> &str {
        &self.current_directory
    }
}
