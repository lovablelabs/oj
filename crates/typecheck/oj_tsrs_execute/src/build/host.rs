// Port of execute/build/host.go.

use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, SystemTime};

use rustc_hash::FxHashMap;
use tsrs_ast::{SourceFile, SourceFileParseOptions};
use tsrs_compiler::CompilerHost;
use tsrs_core::P;
use tsrs_core::collections::SyncMap;
use tsrs_core::tspath::{self, Path};
use tsrs_diagnostics::Message;
use tsrs_incremental::{BuildInfo, BuildInfoReader};
use tsrs_tsoptions::{self as tsoptions, CompilerOptionsValue, ParseConfigHost, ParsedCommandLine};
use tsrs_vfs::FS;

use super::orchestrator::Orchestrator;
use super::parsecache::ParseCache;
use crate::tsc::ExtendedConfigCache;

pub type MTimeCache = Arc<SyncMap<Path, Option<SystemTime>>>;

pub struct HostImpl {
    pub(crate) orchestrator: OnceLock<&'static Orchestrator>,
    pub(crate) host: Arc<dyn CompilerHost>,
    // The caching filesystem under `host` (Go `o.host.host.FS().(*cachedvfs.FS)`), cleared by `resetCaches`.
    pub(crate) cached_fs: Arc<tsrs_vfs::cachedvfs::FS<Arc<dyn tsrs_vfs::FS>>>,

    // Caches that last only for build cycle and then cleared out
    pub(crate) extended_config_cache: Mutex<Arc<ExtendedConfigCache>>,
    pub(crate) source_files: ParseCache<SourceFileParseOptions, Option<P<SourceFile>>>,
    pub(crate) config_times: Mutex<FxHashMap<Path, Duration>>,

    // caches that stay as long as they are needed
    pub(crate) resolved_references: ParseCache<Path, Option<P<ParsedCommandLine>>>,
    // Read by every builder for every input and output; written only when a build or watch cycle starts over.
    pub(crate) m_times: RwLock<MTimeCache>,

    // tsrs-only, watch mode: every parsed file and the modification time it was read at, kept across build cycles
    // (`get_watch_source_file`).
    pub(crate) watch_source_files: Option<
        Mutex<FxHashMap<SourceFileParseOptions, (P<SourceFile>, Option<std::time::SystemTime>)>>,
    >,
    // tsrs-only, watch mode: the regions of those files (watchmem.rs; `Orchestrator::free_unreachable_watch_files`).
    // A build that frees its programs (`Orchestrator::frees_built_programs`): the regions of the cached `source_files`.
    pub(crate) watch_file_regions: crate::watchmem::FileRegions,
}

impl HostImpl {
    // tsrs-only: Go's watch mode reparses every file each cycle (`resetCaches` drops the parse cache, which keeps only
    // declaration and JSON files within a cycle). A watch session here keeps every parsed file and reuses it while the
    // file's text is unchanged, as the non-build watcher does (watcher.rs `watchCompilerHost`): the same text and
    // parse options give the same tree, and binding does not depend on the program.
    //
    // A file whose modification time is the one it was read at is reused without reading it again (as the non-build
    // watcher does); reading and comparing every file's text took about 100 ms of a vscode cycle. A file whose time
    // changed is compared by text.
    fn get_watch_source_file(
        &self,
        cache: &Mutex<
            FxHashMap<SourceFileParseOptions, (P<SourceFile>, Option<std::time::SystemTime>)>,
        >,
        opts: SourceFileParseOptions,
    ) -> Option<P<SourceFile>> {
        let cached = cache.lock().unwrap().get(&opts).copied();
        // A bundled lib is text in the binary.
        if let Some((file, _)) = cached
            && tsrs_vfs::bundled::embedded_file(&opts.file_name).is_some()
        {
            return Some(file);
        }
        // Before reading: a write after this stat changes the time again, and the next cycle reads the file.
        let info = self.host.fs().stat(&opts.file_name);
        let mod_time = info.as_ref().and_then(tsrs_vfs::FileInfo::mod_time);
        if let Some((file, cached_time)) = cached {
            if mod_time.is_some() && mod_time == cached_time {
                return Some(file);
            }
            match self.host.fs().read_file(&opts.file_name) {
                Some(text) if text == file.text() => {
                    cache.lock().unwrap().insert(opts, (file, mod_time));
                    return Some(file);
                }
                Some(_) => {}
                None => {
                    cache.lock().unwrap().remove(&opts);
                    return None;
                }
            }
        }
        let size = info.as_ref().map_or(0, |info| usize::try_from(info.size()).unwrap_or(0));
        let file =
            self.watch_file_regions.parse(size, || self.host.get_source_file(opts.clone()))?;
        cache.lock().unwrap().insert(opts, (file, mod_time));
        Some(file)
    }

    fn o(&self) -> &'static Orchestrator {
        self.orchestrator.get().unwrap()
    }

    // host.go:118
    fn load_or_store_m_time(
        &self,
        file: &str,
        old_cache: Option<&MTimeCache>,
        store: bool,
    ) -> Option<SystemTime> {
        let path = self.o().to_path(file);
        let m_times = self.m_times.read().unwrap().clone();
        if let Some(existing) = m_times.load(&path) {
            return existing;
        }
        let found = old_cache.and_then(|old_cache| old_cache.load(&path));
        let m_time = match found {
            Some(m_time) => m_time,
            None => tsrs_incremental::get_m_time(&*self.host, file),
        };
        if store {
            return m_times.load_or_store(path, m_time).0;
        }
        m_time
    }

    // host.go:137
    pub(crate) fn store_m_time(&self, file: &str, m_time: SystemTime) {
        let path = self.o().to_path(file);
        self.m_times.read().unwrap().store(path, Some(m_time));
    }

    // host.go:142
    pub(crate) fn store_m_time_from_old_cache(&self, file: &str, old_cache: &MTimeCache) {
        let path = self.o().to_path(file);
        if let Some(m_time) = old_cache.load(&path) {
            self.m_times.read().unwrap().store(path, m_time);
        }
    }

    // host.go:106
    pub(crate) fn get_m_time(&self, file: &str) -> Option<SystemTime> {
        self.load_or_store_m_time(file, None, true)
    }

    // host.go:110
    pub(crate) fn set_m_time(&self, file: &str, m_time: SystemTime) -> Result<(), String> {
        CompilerHost::fs(self).chtimes(file, m_time, m_time)
    }
}

impl ParseConfigHost for HostImpl {
    fn fs(&self) -> &dyn FS {
        self.host.fs()
    }

    fn get_current_directory(&self) -> &str {
        self.host.get_current_directory()
    }
}

impl CompilerHost for HostImpl {
    fn fs(&self) -> &dyn FS {
        self.host.fs()
    }

    fn default_library_path(&self) -> &str {
        self.host.default_library_path()
    }

    fn get_current_directory(&self) -> &str {
        self.host.get_current_directory()
    }

    fn trace(&self, _msg: &'static Message, _args: &[&dyn std::fmt::Display]) {
        panic!(
            "build.Orchestrator.host does not support tracing; use a different host for tracing"
        );
    }

    // host.go:55
    fn get_source_file(&self, opts: SourceFileParseOptions) -> Option<P<SourceFile>> {
        if let Some(cache) = &self.watch_source_files {
            return self.get_watch_source_file(cache, opts);
        }
        if tspath::is_declaration_file_name(&opts.file_name)
            || tspath::file_extension_is(&opts.file_name, tspath::EXTENSION_JSON)
        {
            // Cache dts and json files as they will be reused
            return self.source_files.load_or_store(
                &opts,
                |opts| {
                    if !self.o().frees_built_programs() {
                        return self.host.get_source_file(opts.clone());
                    }
                    // tsrs-only: every project that reads the file shares it, so it must outlive the program that
                    // parsed it, which is freed once built (`BuildTask::free_built_program`): a region of its own,
                    // where it is bound and its lazily filled data goes too. Kept for the build, as the cache is.
                    let size = self
                        .host
                        .fs()
                        .stat(&opts.file_name)
                        .map_or(0, |info| usize::try_from(info.size()).unwrap_or(0));
                    self.watch_file_regions.parse(size, || self.host.get_source_file(opts.clone()))
                },
                false, /* allowZero */
            );
        }
        self.host.get_source_file(opts)
    }

    // host.go:62
    fn get_content_mapped_source_files(
        &self,
        _parse_options: SourceFileParseOptions,
        _mapper: &tsrs_contentmapper::Mapper,
    ) -> Result<tsrs_contentmapper::SourceFiles, tsrs_contentmapper::Error> {
        Err(tsrs_contentmapper::Error::ProjectUnavailable)
    }

    // host.go:66
    fn content_mapper_project(&self) -> Option<Arc<dyn tsrs_contentmapper::Project>> {
        panic!(
            "build.Orchestrator.host does not support content mapper project; use an individual project's compiler host instead"
        );
    }

    // host.go:71
    fn get_resolved_project_reference(
        &self,
        file_name: &str,
        path: Path,
    ) -> Option<P<ParsedCommandLine>> {
        self.resolved_references.load_or_store(
            &path,
            |path| {
                let o = self.o();
                let config_start = crate::tsc::DiagnosticTimer::new(
                    o.opts.sys,
                    o.opts.command.compiler_options.diagnostics_enabled(),
                );
                // Wrap command line options in "compilerOptions" key to match tsconfig.json structure
                let command_line_raw = match &o.opts.command.raw {
                    CompilerOptionsValue::Object(_) => {
                        let mut wrapped = tsrs_core::collections::OrderedMap::default();
                        wrapped.insert("compilerOptions".to_string(), o.opts.command.raw.clone());
                        Some(CompilerOptionsValue::Object(wrapped))
                    }
                    _ => None,
                };
                let this: &'static HostImpl = o.host();
                let extended_config_cache = self.extended_config_cache.lock().unwrap().clone();
                let (command_line, _) = tsoptions::get_parsed_command_line_of_config_file_path(
                    file_name,
                    path.clone(),
                    Some(&o.opts.command.compiler_options),
                    command_line_raw.as_ref(),
                    this,
                    Some(&*extended_config_cache),
                );
                let config_time = config_start.elapsed();
                self.config_times.lock().unwrap().insert(path.clone(), config_time);
                command_line.map(P::new)
            },
            true, /* allowZero */
        )
    }
}

impl BuildInfoReader for HostImpl {
    // host.go:99
    fn read_build_info(&self, config: &ParsedCommandLine) -> Option<BuildInfo> {
        let o = self.o();
        let config_path = o.to_path(config.config_name());
        let task = o.get_task(&config_path);
        let (build_info, _) = task.load_or_store_build_info(
            o,
            &o.to_path(config.config_name()),
            &config.get_build_info_file_name(),
        );
        build_info.map(|b| (*b).clone())
    }
}

// `incremental.Host` over the orchestrator's host (Go passes `orchestrator.host` itself).
pub struct IncrementalHost(pub(crate) &'static HostImpl);

impl tsrs_incremental::Host for IncrementalHost {
    fn fs(&self) -> &dyn FS {
        self.0.host.fs()
    }

    fn get_m_time(&self, file_name: &str) -> Option<SystemTime> {
        self.0.get_m_time(file_name)
    }

    fn set_m_time(&self, file_name: &str, m_time: Option<SystemTime>) -> Result<(), String> {
        self.0.set_m_time(file_name, m_time.unwrap_or(SystemTime::UNIX_EPOCH))
    }
}
