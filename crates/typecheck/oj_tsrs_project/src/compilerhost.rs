use std::fmt::Display;
use std::sync::{Arc, OnceLock, RwLock};

use tsrs_ast::{SourceFile, SourceFileParseOptions};
use tsrs_compiler::CompilerHost;
use tsrs_contentmapper as contentmapper;
use tsrs_core::P;
use tsrs_core::context::{Context, locale_from_context};
use tsrs_core::tspath::Path;
use tsrs_diagnostics::Message;
use tsrs_tsoptions::ParsedCommandLine;
use tsrs_vfs::FS;

use crate::configfileregistry::ConfigFileRegistry;
use crate::configfileregistrybuilder::ConfigFileRegistryBuilder;
use crate::logging::LogTree;
use crate::parsecache::{
    ContentMappedParseCache, ParseCache, ParseCacheJournal, content_mapped_parse_cache_key,
    new_parse_cache_key,
};
use crate::project::{ID, Project};
use crate::projectcollectionbuilder::ProjectCollectionBuilder;
use crate::session::SessionOptions;
use crate::snapshotfs::{FileHandleSource, SnapshotFS, SourceFS, new_source_fs};

// The parts of the ProjectCollectionBuilder the host uses while the program is built (Go keeps the whole
// builder pointer and clears it in `freeze`).
pub struct HostBuilder {
    parse_cache: Arc<ParseCache>,
    content_mapped_parse_cache: Arc<ContentMappedParseCache>,
    parse_cache_journal: Arc<ParseCacheJournal>,
    config_file_registry_builder: Arc<ConfigFileRegistryBuilder>,
    content_mapper_host: Option<Arc<dyn contentmapper::Host>>,
    ctx: Context,
}

// compilerhost.go:21 (Go's configFilePath field is never read)
pub struct CompilerHostImpl {
    current_directory: String,
    pub(crate) session_options: Arc<SessionOptions>,

    pub(crate) source_fs: Arc<SourceFS>,
    config_file_registry: RwLock<Option<Arc<ConfigFileRegistry>>>,

    // Go keeps `project *Project`, the project being updated, and reads its ID and (in ContentMapperProject) its
    // command line with typings files, which the project memoizes before its program is built.
    project: RwLock<Option<(ID, Option<P<ParsedCommandLine>>)>>,
    builder: RwLock<Option<HostBuilder>>,
    logger: RwLock<LogTree>,
    // Go: contentMapperProject + contentMapperOnce.
    content_mapper_project: OnceLock<Option<Arc<dyn contentmapper::Project>>>,
}

// compilerhost.go:36
pub fn new_compiler_host(
    current_directory: &str,
    project: &Project,
    builder: &ProjectCollectionBuilder,
    logger: LogTree,
) -> Arc<CompilerHostImpl> {
    Arc::new(CompilerHostImpl {
        current_directory: current_directory.to_string(),
        session_options: Arc::clone(&builder.session_options),

        source_fs: Arc::new(new_source_fs(
            true,
            Arc::<crate::snapshotfs::SnapshotFSBuilder>::clone(&builder.fs),
            Arc::clone(&builder.to_path),
        )),
        config_file_registry: RwLock::new(None),

        project: RwLock::new(Some((project.id(), project.get_command_line_with_typings_files()))),
        builder: RwLock::new(Some(HostBuilder {
            parse_cache: Arc::clone(&builder.parse_cache),
            content_mapped_parse_cache: Arc::clone(&builder.content_mapped_parse_cache),
            parse_cache_journal: Arc::clone(&builder.parse_cache_journal),
            config_file_registry_builder: Arc::clone(&builder.config_file_registry_builder),
            content_mapper_host: builder.content_mapper_host.clone(),
            ctx: builder.ctx.clone(),
        })),
        logger: RwLock::new(logger),
        content_mapper_project: OnceLock::new(),
    })
}

impl CompilerHostImpl {
    // compilerhost.go:57
    // freeze clears references to mutable state to make the compilerHost safe for use
    // after the snapshot has been finalized. See the usage in snapshot.go for more details.
    pub(crate) fn freeze(
        &self,
        snapshot_fs: Arc<SnapshotFS>,
        config_file_registry: Arc<ConfigFileRegistry>,
    ) {
        let mut builder = self.builder.write().unwrap();
        assert!(!builder.is_none(), "freeze can only be called once");
        self.source_fs.set_source(snapshot_fs);
        self.source_fs.disable_tracking();
        *self.config_file_registry.write().unwrap() = Some(config_file_registry);
        *builder = None;
        *self.project.write().unwrap() = None;
        *self.logger.write().unwrap() = LogTree::nil();
    }

    // compilerhost.go:69
    fn ensure_alive(&self) {
        assert!(
            !(self.builder.read().unwrap().is_none() || self.project.read().unwrap().is_none()),
            "method must not be called after snapshot initialization"
        );
    }

    /// The builder's parse cache and the journal of the references its programs take.
    pub(crate) fn builder_parse_cache_journal(&self) -> (Arc<ParseCache>, Arc<ParseCacheJournal>) {
        let builder = self.builder.read().unwrap();
        let builder =
            builder.as_ref().expect("method must not be called after snapshot initialization");
        (Arc::clone(&builder.parse_cache), Arc::clone(&builder.parse_cache_journal))
    }

    /// The builder's content-mapped parse cache (Go `c.builder.contentMappedParseCache`).
    pub(crate) fn builder_content_mapped_parse_cache(&self) -> Arc<ContentMappedParseCache> {
        let builder = self.builder.read().unwrap();
        let builder =
            builder.as_ref().expect("method must not be called after snapshot initialization");
        Arc::clone(&builder.content_mapped_parse_cache)
    }

    pub(crate) fn builder_ctx(&self) -> Context {
        self.builder.read().unwrap().as_ref().map(|b| b.ctx.clone()).unwrap_or_default()
    }
}

impl CompilerHost for CompilerHostImpl {
    // compilerhost.go:81
    fn fs(&self) -> &dyn FS {
        &*self.source_fs
    }

    // compilerhost.go:76
    fn default_library_path(&self) -> &str {
        &self.session_options.default_library_path
    }

    // compilerhost.go:86
    fn get_current_directory(&self) -> &str {
        &self.current_directory
    }

    // compilerhost.go:91
    fn get_resolved_project_reference(
        &self,
        file_name: &str,
        path: Path,
    ) -> Option<P<ParsedCommandLine>> {
        // Config files outlive the program that asks for them (the registry keeps them): never in the program's
        // memory region.
        let _arena = tsrs_core::arena::enter_thread_arena();
        let builder = self.builder.read().unwrap();
        match builder.as_ref() {
            None => self.config_file_registry.read().unwrap().as_ref().unwrap().get_config(&path),
            Some(builder) => {
                // acquireConfigForProject will bypass sourceFS, so track the file here.
                self.source_fs.track(file_name);
                let project = self.project.read().unwrap().as_ref().unwrap().0.clone();
                let logger = self.logger.read().unwrap().clone();
                builder
                    .config_file_registry_builder
                    .acquire_config_for_project(file_name, &path, &project, &logger)
            }
        }
    }

    // compilerhost.go:103
    // GetSourceFile implements compiler.CompilerHost. Files are cached in parseCache
    // and acquired immediately for the in-progress program.
    fn get_source_file(&self, opts: SourceFileParseOptions) -> Option<P<SourceFile>> {
        self.ensure_alive();
        let fh = self.source_fs.get_file_by_path(&opts.file_name, &opts.path)?;
        let key = new_parse_cache_key(opts, fh.hash(), fh.kind());
        let (cache, journal) = self.builder_parse_cache_journal();
        Some(journal.acquire(&cache, key, fh))
    }

    // compilerhost.go:113
    // GetContentMappedSourceFile implements compiler.CompilerHost.
    fn get_content_mapped_source_files(
        &self,
        parse_options: SourceFileParseOptions,
        mapper: &contentmapper::Mapper,
    ) -> Result<contentmapper::SourceFiles, contentmapper::Error> {
        self.ensure_alive();
        let Some(fh) =
            self.source_fs.get_file_by_path(&parse_options.file_name, &parse_options.path)
        else {
            return Ok(contentmapper::SourceFiles::default());
        };
        let diagnostic_locale = locale_from_context(&self.builder_ctx());
        let Some(project) = self.content_mapper_project() else {
            return Err(contentmapper::Error::ProjectUnavailable);
        };
        let identity = match project.identity(mapper) {
            Ok(identity) => identity,
            Err(err) => {
                return Err(contentmapper::new_transform_error(
                    contentmapper::TransformErrorKind::Project,
                    Some(err),
                )
                .into());
            }
        };
        let transform_identity = xxhash_rust::xxh3::xxh3_128(identity.as_bytes());
        let key = content_mapped_parse_cache_key(
            parse_options.clone(),
            fh.hash(),
            transform_identity,
            &diagnostic_locale,
        );
        let cache = self.builder_content_mapped_parse_cache();
        let (_, journal) = self.builder_parse_cache_journal();
        let content = fh.content();
        let files = journal.acquire_content_mapped(&cache, &key, content.len(), || {
            let files =
                contentmapper::transform_and_parse(parse_options, content, mapper, &*project)?;
            let canonical = files.canonical.unwrap();
            canonical.hash.set(key.hash);
            tsrs_binder::bind_source_file(canonical);
            for &supplemental in &files.supplemental {
                supplemental.hash.set(key.hash);
                tsrs_binder::bind_source_file(supplemental);
            }
            Ok(files)
        })?;
        if let Err(err) =
            contentmapper::check_supplemental_file_name_collisions(&files, |file_name| {
                self.fs().file_exists(file_name)
            })
        {
            journal.deref_content_mapped(&cache, &key);
            return Err(err);
        }
        Ok(files)
    }

    // compilerhost.go:153
    fn content_mapper_project(&self) -> Option<Arc<dyn contentmapper::Project>> {
        self.content_mapper_project
            .get_or_init(|| {
                let builder = self.builder.read().unwrap();
                let content_mapper_host = builder.as_ref()?.content_mapper_host.as_ref()?;
                // References into the command line's own list (an arena object): the host keys the project by
                // their addresses.
                let command_line: &'static ParsedCommandLine =
                    self.project.read().unwrap().as_ref()?.1?.get();
                if command_line.content_mappers().is_empty() {
                    return None;
                }
                content_mapper_host.project(contentmapper::ProjectSpec {
                    config_file_name: command_line.config_name().to_string(),
                    mappers: command_line.content_mappers().iter().collect(),
                    compiler_options: command_line.compiler_options(),
                })
            })
            .clone()
    }

    // compilerhost.go:172
    fn trace(&self, msg: &'static Message, args: &[&dyn Display]) {
        self.logger.read().unwrap().log(&msg.localize(args));
    }
}
