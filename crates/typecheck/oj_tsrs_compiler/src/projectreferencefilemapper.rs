use std::sync::{Arc, OnceLock};

use rustc_hash::{FxHashMap, FxHashSet};
use tsrs_core::collections::SyncMap;
use tsrs_core::tspath::{self, Path};
use tsrs_core::{CompilerOptions, P};
use tsrs_module::{self as module, ResolutionHost, ResolvedProjectReference};
use tsrs_tsoptions::{ParsedCommandLine, SourceOutputAndProjectReference};
use tsrs_vfs::FS;

use crate::host::CompilerHost;
use crate::projectreferencedtsfakinghost::new_project_reference_dts_faking_host;

pub struct ProjectReferenceFileMapper {
    pub(crate) config: P<ParsedCommandLine>,
    pub(crate) use_source_of_project_reference: bool,
    pub(crate) dts_directories: FxHashSet<Path>,

    pub(crate) config_to_project_reference: FxHashMap<Path, Option<P<ParsedCommandLine>>>, // All the resolved references needed
    pub(crate) references_in_config_file: FxHashMap<Path, Vec<Path>>, // Map of config file to its references
    pub(crate) source_to_project_reference: FxHashMap<Path, P<SourceOutputAndProjectReference>>,
    pub(crate) output_dts_to_project_reference: FxHashMap<Path, P<SourceOutputAndProjectReference>>,

    // Store all the realpath from dts in node_modules to source file from project reference needed during parsing so it can be used later
    realpath_dts_to_source: SyncMap<Path, Option<P<SourceOutputAndProjectReference>>>,

    // Go builds a new dts-faking host on every `resolutionHost` call; the mapper is immutable once built, so the
    // host is built once (its caches only memoize file system queries).
    pub(crate) dts_faking_host: OnceLock<&'static dyn ResolutionHost>,
    // The leaked `resolution_host_for` host the file loader created for this mapper's program build
    // (it keeps the compiler host, and with it the build's file system, alive). Freed with the mapper
    // (`SharedProgramData::free`).
    pub(crate) loader_host: Option<&'static dyn ResolutionHost>,
}

pub(crate) struct ProjectReferenceFileMapperBuilder {
    pub(crate) mapper: &'static ProjectReferenceFileMapper,
    pub(crate) host: &'static dyn ResolutionHost,
}

struct CompilerResolutionHost(Arc<dyn CompilerHost>);

impl ResolutionHost for CompilerResolutionHost {
    fn fs(&self) -> &dyn FS {
        self.0.fs()
    }

    fn get_current_directory(&self) -> &str {
        self.0.get_current_directory()
    }
}

pub fn resolution_host_for(host: Arc<dyn CompilerHost>) -> &'static dyn ResolutionHost {
    Box::leak(Box::new(CompilerResolutionHost(host)))
}

pub(crate) fn as_resolved_project_reference(
    config: P<ParsedCommandLine>,
) -> &'static dyn ResolvedProjectReference {
    config.get()
}

impl ProjectReferenceFileMapper {
    pub fn new(
        config: P<ParsedCommandLine>,
        use_source_of_project_reference: bool,
    ) -> ProjectReferenceFileMapper {
        ProjectReferenceFileMapper {
            config,
            use_source_of_project_reference,
            dts_directories: FxHashSet::default(),
            config_to_project_reference: FxHashMap::default(),
            references_in_config_file: FxHashMap::default(),
            source_to_project_reference: FxHashMap::default(),
            output_dts_to_project_reference: FxHashMap::default(),
            realpath_dts_to_source: SyncMap::default(),
            dts_faking_host: OnceLock::new(),
            loader_host: None,
        }
    }

    // projectreferencefilemapper.go:30
    pub(crate) fn resolution_host(
        &'static self,
        host: &'static dyn ResolutionHost,
    ) -> &'static dyn ResolutionHost {
        if self.use_source_of_project_reference && !self.output_dts_to_project_reference.is_empty()
        {
            return *self
                .dts_faking_host
                .get_or_init(|| new_project_reference_dts_faking_host(host, self));
        }
        host
    }

    // projectreferencefilemapper.go:37
    pub(crate) fn root_config_path(&self) -> Path {
        match self.config.config_file {
            None => Path::new(""),
            Some(config_file) => config_file.source_file.path().clone(),
        }
    }

    // projectreferencefilemapper.go:44
    pub(crate) fn get_parse_file_redirect(&self, _file_name: &str, path: &Path) -> String {
        if self.use_source_of_project_reference {
            // Map to source file from project reference
            let mut source = self.get_project_reference_from_output_dts(path);
            if source.is_none() {
                source = self.realpath_dts_to_source.load(path).flatten();
            }
            if let Some(source) = source {
                return source.source.clone();
            }
        } else {
            // Map to dts file from project reference
            if let Some(output) = self.get_project_reference_from_source(path)
                && !output.output_dts.is_empty()
            {
                return output.output_dts.clone();
            }
        }
        String::new()
    }

    // projectreferencefilemapper.go:64
    pub(crate) fn get_resolved_project_references(&self) -> Vec<Option<P<ParsedCommandLine>>> {
        let mut result = Vec::new();
        if let Some(refs) = self.references_in_config_file.get(&self.root_config_path()) {
            result.reserve(refs.len());
            result.extend(
                refs.iter().map(|ref_path| {
                    self.config_to_project_reference.get(ref_path).copied().flatten()
                }),
            );
        }
        result
    }

    // projectreferencefilemapper.go:77
    pub(crate) fn get_project_reference_from_source(
        &self,
        path: &Path,
    ) -> Option<P<SourceOutputAndProjectReference>> {
        self.source_to_project_reference.get(path).copied()
    }

    // projectreferencefilemapper.go:81
    pub(crate) fn get_project_reference_from_output_dts(
        &self,
        path: &Path,
    ) -> Option<P<SourceOutputAndProjectReference>> {
        self.output_dts_to_project_reference.get(path).copied()
    }

    // projectreferencefilemapper.go:85
    pub(crate) fn is_source_from_project_reference(&self, path: &Path) -> bool {
        self.use_source_of_project_reference
            && self.get_project_reference_from_source(path).is_some()
    }

    // projectreferencefilemapper.go:89
    pub(crate) fn get_compiler_options_for_file(
        &self,
        file_name: &str,
        path: &Path,
    ) -> P<CompilerOptions> {
        let redirect = self.get_redirect_parsed_command_line_for_resolution(file_name, path);
        module::get_compiler_options_with_redirect(
            self.config.compiler_options().unwrap(),
            redirect.map(as_resolved_project_reference),
        )
    }

    // projectreferencefilemapper.go:94
    pub(crate) fn get_redirect_parsed_command_line_for_resolution(
        &self,
        _file_name: &str,
        path: &Path,
    ) -> Option<P<ParsedCommandLine>> {
        self.redirect_reference(path).map(|reference| reference.resolved)
    }

    // projectreferencefilemapper.go:99
    fn get_redirect_for_resolution(
        &self,
        file_name: &str,
        path: &Path,
    ) -> (Option<P<ParsedCommandLine>>, String) {
        match self.redirect_reference(path) {
            Some(reference) => (Some(reference.resolved), reference.source.clone()),
            None => (None, file_name.to_string()),
        }
    }

    // The reference `get_redirect_for_resolution` answers from, without building its file name string: the checker
    // asks for the compiler options of a file per import and per declaration (module format, emit syntax). A program
    // without references has empty source and output maps and never stores into `realpath_dts_to_source` (see
    // `projectReferenceFileMapperBuilder::has_no_references`), so it skips hashing the path and the shared map's lock.
    fn redirect_reference(&self, path: &Path) -> Option<P<SourceOutputAndProjectReference>> {
        if self.config.resolved_project_reference_paths().is_empty() {
            return None;
        }
        // Check if outputdts of source file from project reference
        if let Some(output) = self.get_project_reference_from_source(path) {
            return Some(output);
        }

        // Source file from project reference
        if let Some(result_from_dts) = self.get_project_reference_from_output_dts(path) {
            return Some(result_from_dts);
        }

        self.realpath_dts_to_source.load(path).flatten()
    }

    // projectreferencefilemapper.go:124
    pub(crate) fn range_resolved_project_reference(
        &self,
        mut f: impl FnMut(&Path, Option<P<ParsedCommandLine>>, P<ParsedCommandLine>, usize) -> bool,
    ) -> bool {
        if self.config.project_references().is_empty() {
            return false;
        }
        let mut seen_ref: FxHashSet<Path> = FxHashSet::with_capacity_and_hasher(
            self.references_in_config_file.len(),
            rustc_hash::FxBuildHasher,
        );
        let root_config_path = self.root_config_path();
        seen_ref.insert(root_config_path.clone());
        let refs = self
            .references_in_config_file
            .get(&root_config_path)
            .map_or(&[] as &[Path], std::vec::Vec::as_slice);
        self.range_resolved_reference_worker(refs, &mut f, self.config, &mut seen_ref)
    }

    // projectreferencefilemapper.go:137
    fn range_resolved_reference_worker(
        &self,
        references: &[Path],
        f: &mut impl FnMut(&Path, Option<P<ParsedCommandLine>>, P<ParsedCommandLine>, usize) -> bool,
        parent: P<ParsedCommandLine>,
        seen_ref: &mut FxHashSet<Path>,
    ) -> bool {
        if references.iter().enumerate().any(|(index, path)| {
            if !seen_ref.insert(path.clone()) {
                return false;
            }
            let config = self.config_to_project_reference.get(path).copied().flatten();
            if !f(path, config, parent, index) {
                return true;
            }
            let child_refs = self
                .references_in_config_file
                .get(path)
                .map_or(&[] as &[Path], std::vec::Vec::as_slice);
            // A reference that did not resolve has no references of its own (Go recurses with a nil parent over
            // an empty list).
            if let Some(config) = config
                && !self.range_resolved_reference_worker(child_refs, f, config, seen_ref)
            {
                return true;
            }

            false
        }) {
            return false;
        }
        true
    }

    // projectreferencefilemapper.go:159
    pub(crate) fn range_resolved_project_reference_in_child_config(
        &self,
        child_config: Option<P<ParsedCommandLine>>,
        mut f: impl FnMut(&Path, Option<P<ParsedCommandLine>>, P<ParsedCommandLine>, usize) -> bool,
    ) -> bool {
        let Some(child_config) = child_config else {
            return false;
        };
        let Some(child_config_file) = child_config.config_file else {
            return false;
        };
        let mut seen_ref: FxHashSet<Path> = FxHashSet::with_capacity_and_hasher(
            self.references_in_config_file.len(),
            rustc_hash::FxBuildHasher,
        );
        seen_ref.insert(child_config_file.source_file.path().clone());
        let refs = self
            .references_in_config_file
            .get(child_config_file.source_file.path())
            .map_or(&[] as &[Path], std::vec::Vec::as_slice);
        self.range_resolved_reference_worker(refs, &mut f, self.config, &mut seen_ref)
    }
}

impl ProjectReferenceFileMapperBuilder {
    pub(crate) fn take_mapper(&self) -> &'static ProjectReferenceFileMapper {
        self.mapper
    }

    // projectreferencefilemapper.go:174
    pub(crate) fn get_parse_file_redirect(&self, file_name: &str, path: &Path) -> String {
        if self.has_no_references() {
            return String::new();
        }
        if self.mapper.use_source_of_project_reference
            && self.mapper.get_project_reference_from_output_dts(path).is_none()
        {
            self.resolve_symlink(file_name, path);
        }
        self.mapper.get_parse_file_redirect(file_name, path)
    }

    // projectreferencefilemapper.go:181
    pub(crate) fn get_redirect_for_resolution(
        &self,
        file_name: &str,
        path: &Path,
    ) -> (Option<P<ParsedCommandLine>>, String) {
        if self.has_no_references() {
            return (None, file_name.to_string());
        }
        if self.mapper.get_project_reference_from_source(path).is_none()
            && self.mapper.get_project_reference_from_output_dts(path).is_none()
        {
            self.resolve_symlink(file_name, path);
        }
        self.mapper.get_redirect_for_resolution(file_name, path)
    }

    // projectreferencefilemapper.go:188
    pub(crate) fn get_compiler_options_for_file(
        &self,
        file_name: &str,
        path: &Path,
    ) -> P<CompilerOptions> {
        let (redirect, _) = self.get_redirect_for_resolution(file_name, path);
        module::get_compiler_options_with_redirect(
            self.mapper.config.compiler_options().unwrap(),
            redirect.map(as_resolved_project_reference),
        )
    }

    // projectreferencefilemapper.go:193
    pub(crate) fn get_redirect_parsed_command_line_for_resolution(
        &self,
        file_name: &str,
        path: &Path,
    ) -> Option<P<ParsedCommandLine>> {
        let (redirect, _) = self.get_redirect_for_resolution(file_name, path);
        redirect
    }

    // tsrs-only: a program without project references has empty source and output maps, and `resolve_symlink` stores
    // into `realpath_dts_to_source` only with references, so every redirect query misses. Answering that up front
    // skips hashing the file's path into three maps (one of them a shared, locked map) per file and per import.
    fn has_no_references(&self) -> bool {
        self.mapper.config.resolved_project_reference_paths().is_empty()
    }

    // projectreferencefilemapper.go:198
    fn resolve_symlink(&self, file_name: &str, path: &Path) {
        // If preserveSymlinks is true, module resolution wont jump the symlink
        // but the resolved real path may be the .d.ts from project reference
        // Note:: Currently we try the real path only if the
        // file is from node_modules to avoid having to run real path on all file paths
        if self.mapper.realpath_dts_to_source.load(path).is_some() {
            return;
        }
        if !self.mapper.config.resolved_project_reference_paths().is_empty()
            && self.mapper.config.compiler_options().unwrap().preserve_symlinks.is_true()
        {
            if file_name.contains("/node_modules/") {
                let real_declaration_path = tspath::to_path(
                    &self.host.fs().realpath(file_name),
                    self.host.get_current_directory(),
                    self.host.fs().use_case_sensitive_file_names(),
                );
                if real_declaration_path == *path {
                    self.mapper.realpath_dts_to_source.store(path.clone(), None);
                } else {
                    self.mapper.realpath_dts_to_source.store(
                        path.clone(),
                        self.mapper.get_project_reference_from_output_dts(&real_declaration_path),
                    );
                }
            } else {
                self.mapper.realpath_dts_to_source.store(path.clone(), None);
            }
        }
    }
}
