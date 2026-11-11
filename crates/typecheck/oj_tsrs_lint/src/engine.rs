//! Program construction and headless lint execution.

use std::collections::{BTreeMap, VecDeque};
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use rustc_hash::{FxHashMap, FxHashSet};

use tsrs_ast::{Diagnostic as TsDiagnostic, SourceFile, SourceFileParseOptions};
use tsrs_compiler::{CompilerHost, Context, Program, ProgramOptions};
use tsrs_contentmapper as contentmapper;
use tsrs_core::json::Value;
use tsrs_core::tspath;
use tsrs_core::{
    CompilerOptions, JsxEmit, Locale, ModuleKind, ModuleResolutionKind, P, ScriptTarget, Tristate,
};
use tsrs_tsoptions::{ParseConfigHost, ParsedCommandLine};
use tsrs_vfs::{FS, bundled, cachedvfs, osvfs, wrapvfs};

use crate::payload::{ConfiguredRule, Payload};
use crate::protocol::{self, Diagnostic, MessageType, Range, RuleMessage};
use crate::rule::{RuleContext, visit_lint_nodes};

#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    pub fix: bool,
    pub fix_suggestions: bool,
    pub debug_timings: bool,
    pub suppress_program_diagnostics: bool,
    /// tsc's `--runExternalCode`: tsconfig `contentMappers` are kept and their mapper processes run.
    pub run_external_code: bool,
}

#[derive(Default)]
struct RequestRegions {
    loaders: Arc<tsrs_compiler::ProgramRegions>,
}

/// What the programs of one request share, as the projects of a `tsc -b` do. Each value is made once, in a registered
/// region of its own kept until the request ends, so what it fills lazily goes there too (`arena::enter_owner`).
#[derive(Default)]
struct Shared {
    // By the name asked for, which is absolute: a command line keeps the spelling it was parsed from.
    references: tsrs_core::collections::SyncMap<String, Option<P<ParsedCommandLine>>>,
    files: tsrs_core::collections::SyncMap<SourceFileParseOptions, Option<P<SourceFile>>>,
    regions: Mutex<Vec<tsrs_core::arena::Region>>,
}

impl Shared {
    fn keep<T>(&self, first_chunk: usize, make: impl FnOnce() -> T) -> T {
        let region = tsrs_core::arena::Region::new(first_chunk);
        let _scope = region.enter();
        let value = make();
        region.trim();
        self.regions.lock().unwrap().push(region);
        value
    }
}

struct Host {
    host: Arc<dyn CompilerHost>,
    shared: Arc<Shared>,
}

impl CompilerHost for Host {
    fn fs(&self) -> &dyn FS {
        self.host.fs()
    }
    fn default_library_path(&self) -> &str {
        self.host.default_library_path()
    }
    fn get_current_directory(&self) -> &str {
        self.host.get_current_directory()
    }
    fn trace(&self, msg: &'static tsrs_diagnostics::Message, args: &[&dyn std::fmt::Display]) {
        self.host.trace(msg, args);
    }
    fn get_source_file(&self, opts: SourceFileParseOptions) -> Option<P<SourceFile>> {
        let name = &opts.file_name;
        if !tspath::is_declaration_file_name(name) {
            return self.host.get_source_file(opts);
        }
        if let Some(file) = self.shared.files.load(&opts) {
            return file;
        }
        // Text, tree and binder data take about 8 times the text (fileregions.rs).
        let size = self.fs().stat(name).map_or(0, |info| usize::try_from(info.size()).unwrap_or(0));
        let file = self.shared.keep(size.saturating_mul(10).min(64 << 20), || {
            // Bound here, in its region: a program that bound it later would bind it in a region of its own.
            self.host.get_source_file(opts.clone()).inspect(|&f| tsrs_binder::bind_source_file(f))
        });
        self.shared.files.load_or_store(opts, file).0
    }
    fn get_content_mapped_source_files(
        &self,
        parse_options: SourceFileParseOptions,
        mapper: &contentmapper::Mapper,
    ) -> Result<contentmapper::SourceFiles, contentmapper::Error> {
        self.host.get_content_mapped_source_files(parse_options, mapper)
    }
    fn content_mapper_project(&self) -> Option<Arc<dyn contentmapper::Project>> {
        self.host.content_mapper_project()
    }
    fn get_resolved_project_reference(
        &self,
        file_name: &str,
        path: tspath::Path,
    ) -> Option<P<ParsedCommandLine>> {
        if let Some(config) = self.shared.references.load(file_name) {
            return config;
        }
        let config =
            self.shared.keep(0, || self.host.get_resolved_project_reference(file_name, path));
        self.shared.references.load_or_store(file_name.into(), config).0
    }
}

pub struct ConfigHost {
    fs: Arc<dyn FS>,
    cwd: String,
    // The command-line options every tsconfig is parsed with.
    existing_options: Option<CompilerOptions>,
}

impl ConfigHost {
    pub fn new(fs: Arc<dyn FS>, cwd: String) -> Self {
        Self { fs, cwd, existing_options: None }
    }
}

impl ParseConfigHost for ConfigHost {
    fn fs(&self) -> &dyn FS {
        &*self.fs
    }
    fn get_current_directory(&self) -> &str {
        &self.cwd
    }
}

pub fn run(
    payload: &Payload,
    options: Options,
    cwd: &str,
    writer: &mut dyn Write,
) -> Result<(), String> {
    let overrides = Arc::new(payload.source_overrides.clone());
    let read_overrides = Arc::clone(&overrides);
    let directory_overrides = Arc::clone(&overrides);
    let entries_overrides = Arc::clone(&overrides);
    let stat_overrides = Arc::clone(&overrides);
    let realpath_overrides = Arc::clone(&overrides);
    let read_cache = tsrs_core::collections::SyncMap::<String, Option<String>>::default();
    let fs = wrapvfs::wrap(
        osvfs::fs(),
        wrapvfs::Replacements {
            file_exists: Some(Box::new(move |path| {
                overrides.contains_key(path) || osvfs::fs().file_exists(path)
            })),
            read_file: Some(Box::new(move |path| {
                if let Some(source) = read_overrides.get(path) {
                    return Some(source.clone());
                }
                if let Some(source) = read_cache.load(path) {
                    return source;
                }
                let source = osvfs::fs().read_file(path);
                read_cache.store(path.to_owned(), source.clone());
                source
            })),
            directory_exists: Some(Box::new(move |path| {
                if !directory_overrides.is_empty() {
                    let prefix = format!("{}/", tspath::normalize_path(path).trim_end_matches('/'));
                    if directory_overrides.keys().any(|file| file.starts_with(&prefix)) {
                        return true;
                    }
                }
                osvfs::fs().directory_exists(path)
            })),
            get_accessible_entries: Some(Box::new(move |path| {
                let mut entries = osvfs::fs().get_accessible_entries(path);
                if !entries_overrides.is_empty() {
                    let prefix = format!("{}/", tspath::normalize_path(path).trim_end_matches('/'));
                    for file in entries_overrides.keys() {
                        if let Some(relative) = file.strip_prefix(&prefix) {
                            if let Some((directory, _)) = relative.split_once('/') {
                                entries.directories.push(directory.into());
                            } else {
                                entries.files.push(relative.into());
                            }
                        }
                    }
                    entries.files.sort_unstable();
                    entries.files.dedup();
                    entries.directories.sort_unstable();
                    entries.directories.dedup();
                }
                entries
            })),
            stat: Some(Box::new(move |path| {
                stat_overrides.get(path).map_or_else(
                    || osvfs::fs().stat(path),
                    |source| {
                        Some(tsrs_vfs::FileInfo {
                            name: path.into(),
                            size: i64::try_from(source.len()).unwrap(),
                            mode: tsrs_vfs::FileMode::None,
                            mod_time: None,
                        })
                    },
                )
            })),
            realpath: Some(Box::new(move |path| {
                if realpath_overrides.contains_key(path) {
                    path.into()
                } else {
                    osvfs::fs().realpath(path)
                }
            })),
            ..Default::default()
        },
    );
    let fs: Arc<dyn FS> = Arc::new(bundled::wrap_fs(cachedvfs::from(Arc::new(fs))));
    let host = ConfigHost {
        existing_options: options
            .run_external_code
            .then(|| CompilerOptions { run_external_code: Tristate::True, ..Default::default() }),
        ..ConfigHost::new(Arc::clone(&fs), cwd.into())
    };
    let region = tsrs_core::arena::Region::new_scratch(1 << 20);
    // The inferred program's loader regions: each tsconfig program has its own.
    let regions = RequestRegions::default();
    let _scope = region.enter();
    run_in_region(payload, options, cwd, writer, fs, &host, &regions)
}

struct ProgramOwner {
    program: &'static Program,
    // Dropped after the program is freed.
    _content_mappers: Option<ContentMappers>,
}

impl Drop for ProgramOwner {
    fn drop(&mut self) {
        // SAFETY: each program is created once here, is never shared with another version, and all checker handles have been dropped.
        unsafe {
            tsrs_compiler::free_unshared_program(self.program);
        }
    }
}

// A program's content mapper project and host, closed in that order as the CLI's `ContentMapperCloser` does: a host
// that is still open keeps its mapper processes running.
struct ContentMappers {
    host: Arc<dyn contentmapper::Host>,
    project: Option<Arc<dyn contentmapper::Project>>,
}

impl ContentMappers {
    // The CLI's `new_content_mapper_host` and `get_content_mapper_project` (tsc.go:394), without a host for a
    // program that has no mappers.
    fn open(config: P<ParsedCommandLine>) -> Option<Self> {
        let options = config.compiler_options()?;
        // References into the command line's own list: the host keys the project by their addresses.
        let config: &'static ParsedCommandLine = config.get();
        if !options.run_external_code.is_true() || config.content_mappers().is_empty() {
            return None;
        }
        let host = contentmapper::new_host_with_options(
            Arc::new(contentmapper::ProcessSpawner),
            Locale(options.locale.clone()),
            contentmapper::HostOptions::default(),
        );
        let project = host.project(contentmapper::ProjectSpec {
            config_file_name: config.config_name().to_string(),
            mappers: config.content_mappers().iter().collect(),
            compiler_options: Some(options),
        });
        Some(Self { host, project })
    }
}

impl Drop for ContentMappers {
    fn drop(&mut self) {
        if let Some(project) = &self.project {
            let _ = project.close();
        }
        let _ = self.host.close();
    }
}

fn run_in_region(
    payload: &Payload,
    options: Options,
    cwd: &str,
    writer: &mut dyn Write,
    fs: Arc<dyn FS>,
    host: &ConfigHost,
    regions: &RequestRegions,
) -> Result<(), String> {
    let mut programs: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut unmatched = Vec::new();
    let mut file_configs = BTreeMap::new();
    let mut timings: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    for config in &payload.configs {
        for path in &config.file_paths {
            file_configs.insert(tspath::normalize_slashes(path), config.rules.as_slice());
        }
    }
    for rules in file_configs.values() {
        for rule in *rules {
            crate::registry::require_implemented(&rule.name)?;
        }
    }
    // tsrs-only: semantic reporting (oxlint `--type-check`) reports what `tsrs --noEmit` does for each tsconfig program:
    // every file it checks and the program's global diagnostics, where tsgolint reports only the files oxlint sent. One
    // `oxlint --type-aware --type-check` run can then replace a separate typecheck. Files oxlint sent that no tsconfig
    // includes are checked in an inferred program. Each file's type errors are reported once: a listed file by the
    // program it maps to, any other file by the first program that has it.
    let mut type_checked = FxHashSet::default();
    let mut config_cache = FxHashMap::default();
    for path in file_configs.keys() {
        let resolved = tspath::get_normalized_absolute_path(path, cwd);
        if payload.report_semantic {
            type_checked.insert(tspath::to_path(
                &resolved,
                cwd,
                fs.use_case_sensitive_file_names(),
            ));
        }
        if let Some(config_path) = find_tsconfig(&resolved, host, &mut config_cache) {
            programs.entry(config_path).or_default().push(path.clone());
        } else {
            unmatched.push(path.clone());
        }
    }
    drop(config_cache);
    let shared = Arc::default();
    for (config_path, files) in programs {
        // Dropped after the program: nothing that outlives a program points into its loader regions.
        let regions = RequestRegions::default();
        let (config, diagnostics) = tsrs_tsoptions::get_parsed_command_line_of_config_file(
            &config_path,
            host.existing_options.as_ref(),
            None,
            host,
            None,
        );
        if !diagnostics.is_empty() {
            for diagnostic in diagnostics {
                report_config_error(diagnostic, &config_path, false, writer)?;
            }
            continue;
        }
        let config = config.ok_or_else(|| format!("couldn't read tsconfig at {config_path}"))?;
        if !config.errors.is_empty() && !options.suppress_program_diagnostics {
            for diagnostic in &config.errors {
                report_config_error(*diagnostic, &config_path, false, writer)?;
            }
            continue;
        }
        let program = create_program(
            config,
            Arc::clone(&fs),
            &tspath::get_directory_path(&config_path),
            true,
            Arc::clone(&regions.loaders),
            &shared,
        );
        let program_ref = program.program;
        let diagnostics = program_ref.get_program_diagnostics();
        if !diagnostics.is_empty() && !options.suppress_program_diagnostics {
            for diagnostic in diagnostics {
                report_config_error(diagnostic, &config_path, true, writer)?;
            }
            continue;
        }
        let global_diagnostics = run_program(
            program_ref,
            &files,
            payload.report_semantic.then_some(&mut type_checked),
            payload,
            options,
            writer,
            &file_configs,
            &mut timings,
        )?;
        for diagnostic in global_diagnostics {
            report_global_error(diagnostic, &config_path, writer)?;
        }
    }
    if !unmatched.is_empty() {
        let config = tsrs_tsoptions::new_parsed_command_line(
            P::new(CompilerOptions {
                allow_js: Tristate::True,
                module: ModuleKind::ES_NEXT,
                module_resolution: ModuleResolutionKind::BUNDLER,
                target: ScriptTarget::ES2022,
                jsx: JsxEmit::REACT_JSX,
                allow_importing_ts_extensions: Tristate::True,
                strict_null_checks: Tristate::True,
                strict_function_types: Tristate::True,
                source_map: Tristate::True,
                es_module_interop: Tristate::True,
                allow_non_ts_extensions: Tristate::True,
                resolve_json_module: Tristate::True,
                ..Default::default()
            }),
            unmatched.clone(),
            Vec::new(),
            tspath::ComparePathsOptions {
                current_directory: cwd.into(),
                use_case_sensitive_file_names: fs.use_case_sensitive_file_names(),
            },
        );
        let program = create_program(config, fs, cwd, false, Arc::clone(&regions.loaders), &shared);
        run_program(
            program.program,
            &unmatched,
            None,
            payload,
            options,
            writer,
            &file_configs,
            &mut timings,
        )?;
    }
    if options.debug_timings {
        let mut records = timings.into_iter().collect::<Vec<_>>();
        records.sort_by(|(left_name, (left_duration, _)), (right_name, (right_duration, _))| {
            right_duration.cmp(left_duration).then_with(|| left_name.cmp(right_name))
        });
        protocol::write_message(
            writer,
            MessageType::Timing,
            &protocol::object([(
                "rules",
                Value::Array(
                    records
                        .into_iter()
                        .map(|(name, (duration, calls))| {
                            protocol::object([
                                ("rule_name", Value::String(name)),
                                ("duration", Value::Number(duration as f64)),
                                ("calls", Value::Number(calls as f64)),
                            ])
                        })
                        .collect(),
                ),
            )]),
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn create_program(
    mut config: ParsedCommandLine,
    fs: Arc<dyn FS>,
    cwd: &str,
    references: bool,
    loader_regions: Arc<tsrs_compiler::ProgramRegions>,
    shared: &Arc<Shared>,
) -> ProgramOwner {
    if let Some(checkers) = checker_count() {
        config.parsed_config.compiler_options = Some(P::new(CompilerOptions {
            checkers: Some(checkers),
            ..(*config.compiler_options().unwrap()).clone()
        }));
    }
    let config = P::new(config);
    let content_mappers = ContentMappers::open(config);
    // As in the CLI, `TSRS_LIB_PATH` overrides where the default libraries are read from.
    let host = tsrs_compiler::new_compiler_host(
        cwd,
        fs,
        &bundled::default_lib_path(),
        None,
        None,
        content_mappers.as_ref().and_then(|mappers| mappers.project.clone()),
    );
    let host = Arc::new(Host { host, shared: Arc::clone(shared) });
    let mut options = ProgramOptions::new(config, host);
    options.use_source_of_project_reference = references;
    options.regions = Some(loader_regions);
    // The checkers' memory goes back with the program (`free_unshared_program`), before the next program's is built.
    options.checker_regions = true;
    ProgramOwner { program: tsrs_compiler::new_program(options), _content_mappers: content_mappers }
}

/// `RAYON_NUM_THREADS` (oxlint's thread count) caps the checkers as well as the loader threads, else the compiler's
/// default for the machine and program applies.
fn checker_count() -> Option<i64> {
    std::env::var("RAYON_NUM_THREADS").ok().and_then(|value| value.parse().ok()).filter(|&n| n > 0)
}

// Lints the files on the program's checker pool: one thread per checker, each over the files the pool assigned to
// it (by import-graph locality under the fast preset), so a checker resolves the types of its own corner of the
// program instead of nearly all of it. Output keeps the order of `files`.
// `whole` (with semantic reporting): the files that already have a program to report their type errors. Also
// report the compiler diagnostics of the program's files outside that set, after `files`, add them to it, and return
// the program's global diagnostics.
fn run_program(
    program: &'static Program,
    files: &[String],
    whole: Option<&mut FxHashSet<tspath::Path>>,
    payload: &Payload,
    options: Options,
    writer: &mut dyn Write,
    file_configs: &BTreeMap<String, &[ConfiguredRule]>,
    timings: &mut BTreeMap<String, (u64, u64)>,
) -> Result<Vec<P<TsDiagnostic>>, String> {
    program.bind_source_files();
    let report_global = whole.is_some();
    let mut work = Vec::with_capacity(files.len());
    for name in files {
        let file = program
            .get_source_file(name)
            .ok_or_else(|| format!("Expected file '{name}' to be in program"))?;
        let configured = file_configs.get(name).copied().unwrap_or_default();
        if payload.report_syntactic || payload.report_semantic || !configured.is_empty() {
            work.push((file, configured));
        }
    }
    if let Some(type_checked) = whole {
        for &file in program.get_source_files() {
            if !program.skip_type_checking(file, false) && type_checked.insert(file.path().clone())
            {
                work.push((file, &[]));
            }
        }
    }
    if work.is_empty() {
        return Ok(Vec::new());
    }
    let lint_files: Vec<P<SourceFile>> = work.iter().map(|&(file, _)| file).collect();
    let outputs: Vec<Mutex<Vec<u8>>> = work.iter().map(|_| Mutex::new(Vec::new())).collect();
    let errors: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let pool_timings: Mutex<BTreeMap<String, (u64, u64)>> = Mutex::new(BTreeMap::new());
    let mut global_diagnostics = Vec::new();
    // Every file is type-checked before any rule runs: a rule's type query could otherwise hit an error before the
    // checking `tsrs --noEmit` does, and record it without a location (a global diagnostic) instead.
    for rules in [false, true] {
        let pooled = program.for_each_checker_group(&lint_files, |checker, index, file| {
            let mut output = Vec::new();
            let mut file_timings = BTreeMap::new();
            let result = lint_one(
                program,
                file,
                if rules { work[index].1 } else { &[] },
                !rules,
                payload,
                options,
                checker,
                &mut output,
                &mut file_timings,
            );
            if let Err(error) = result {
                errors.lock().unwrap().push(error);
            }
            outputs[index].lock().unwrap().extend(output);
            if options.debug_timings {
                merge_timings(&mut pool_timings.lock().unwrap(), file_timings);
            }
        });
        if !pooled {
            return Err("the program has no checker pool".into());
        }
        if !rules && report_global {
            global_diagnostics = program.get_global_diagnostics(&Context::default());
        }
    }
    if let Some(error) = errors.into_inner().unwrap().into_iter().next() {
        return Err(error);
    }
    for output in outputs {
        writer.write_all(&output.into_inner().unwrap()).map_err(|error| error.to_string())?;
    }
    merge_timings(timings, pool_timings.into_inner().unwrap());
    Ok(global_diagnostics)
}

fn lint_one(
    program: &'static Program,
    file: P<SourceFile>,
    configured: &[ConfiguredRule],
    check: bool,
    payload: &Payload,
    options: Options,
    checker: &mut tsrs_checker::Checker,
    output: &mut Vec<u8>,
    timings: &mut BTreeMap<String, (u64, u64)>,
) -> Result<(), String> {
    let context = Context::default();
    if check && payload.report_syntactic {
        for diagnostic in program.get_syntactic_diagnostics(&context, Some(file)) {
            report_type_error(diagnostic, file, output)?;
        }
    }
    if check && payload.report_semantic {
        let diagnostics = program.get_semantic_diagnostics_with_checker(&context, checker, file);
        for diagnostic in tsrs_compiler::sort_and_deduplicate_diagnostics(&diagnostics) {
            report_type_error(diagnostic, file, output)?;
        }
    }
    // Rules see a content-mapped file's virtual text, whose ranges and fixes do not apply to the file on disk.
    // oxlint does not send such files.
    if configured.is_empty() || file.is_content_mapped() {
        return Ok(());
    }
    for diagnostic in lint_file(program, file, configured, options, checker, timings)? {
        emit(&diagnostic, output)?;
    }
    Ok(())
}

fn merge_timings(into: &mut BTreeMap<String, (u64, u64)>, from: BTreeMap<String, (u64, u64)>) {
    for (name, (duration, calls)) in from {
        let entry = into.entry(name).or_default();
        entry.0 = entry.0.saturating_add(duration);
        entry.1 += calls;
    }
}

fn lint_file(
    program: &'static Program,
    file: P<SourceFile>,
    configured: &[ConfiguredRule],
    options: Options,
    checker: &mut tsrs_checker::Checker,
    timings: &mut BTreeMap<String, (u64, u64)>,
) -> Result<Vec<Diagnostic>, String> {
    let mut diagnostics = Vec::new();
    let mut rules = Vec::with_capacity(configured.len());
    for config in configured {
        let start = options.debug_timings.then(Instant::now);
        let mut rule = crate::registry::create_rule(config)?;
        rule.initialize(&mut RuleContext {
            source_file: file,
            program,
            checker,
            rule_name: &config.name,
            options,
            diagnostics: &mut diagnostics,
        });
        if let Some(start) = start {
            let entry = timings.entry(config.name.clone()).or_default();
            entry.0 = entry
                .0
                .saturating_add(u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX));
            entry.1 += 1;
        }
        rules.push((config.name.as_str(), rule));
    }
    let mut listeners = FxHashMap::default();
    visit_lint_nodes(file, &mut |listener, node| {
        let active: &mut Vec<usize> = listeners.entry(listener).or_insert_with(|| {
            rules
                .iter()
                .enumerate()
                .filter_map(|(index, (_, rule))| rule.listens_to(listener).then_some(index))
                .collect()
        });
        for &index in active.iter() {
            let (name, rule) = &mut rules[index];
            let start = options.debug_timings.then(Instant::now);
            let mut context = RuleContext {
                source_file: file,
                program,
                checker,
                rule_name: name,
                options,
                diagnostics: &mut diagnostics,
            };
            rule.visit(listener, node, &mut context);
            if let Some(start) = start {
                let entry = timings.entry((*name).into()).or_default();
                entry.0 = entry
                    .0
                    .saturating_add(u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX));
                entry.1 += 1;
            }
        }
    });
    Ok(diagnostics)
}

fn report_type_error(
    diagnostic: P<TsDiagnostic>,
    file: P<SourceFile>,
    writer: &mut dyn Write,
) -> Result<(), String> {
    if diagnostic.file().is_none_or(|f| f.file_name() != file.file_name()) {
        return Ok(());
    }
    emit(
        &Diagnostic {
            range: original_range(diagnostic, file),
            message: RuleMessage {
                id: format!("TS{}", diagnostic.code()),
                description: diagnostic.localize(),
                help: String::new(),
            },
            file_path: Some(file.file_name().into()),
            ..Default::default()
        },
        writer,
    )
}

// A diagnostic of the whole program, such as a missing global type, which `tsrs --noEmit` reports without a location:
// reported on the program's tsconfig, like a configuration error.
fn report_global_error(
    diagnostic: P<TsDiagnostic>,
    config_path: &str,
    writer: &mut dyn Write,
) -> Result<(), String> {
    emit(
        &Diagnostic {
            range: diagnostic_range(diagnostic),
            message: RuleMessage {
                id: format!("TS{}", diagnostic.code()),
                description: diagnostic.localize(),
                help: String::new(),
            },
            file_path: Some(
                diagnostic
                    .file()
                    .map_or_else(|| config_path.to_owned(), |f| f.file_name().to_owned()),
            ),
            ..Default::default()
        },
        writer,
    )
}

fn report_config_error(
    diagnostic: P<TsDiagnostic>,
    config_path: &str,
    program_diagnostic: bool,
    writer: &mut dyn Write,
) -> Result<(), String> {
    let mut help = diagnostic.localize();
    if program_diagnostic && help.contains("Please remove it from your configuration.") {
        help.push_str(
            "\nSee https://github.com/oxc-project/tsgolint/issues/351 for more information.",
        );
    }
    emit(
        &Diagnostic {
            range: diagnostic_range(diagnostic),
            message: RuleMessage {
                id: "tsconfig-error".into(),
                description: "Invalid tsconfig".into(),
                help,
            },
            file_path: Some(
                diagnostic
                    .file()
                    .map_or_else(|| config_path.to_owned(), |f| f.file_name().to_owned()),
            ),
            ..Default::default()
        },
        writer,
    )
}

fn diagnostic_range(diagnostic: P<TsDiagnostic>) -> Option<Range> {
    Some(Range { pos: diagnostic.pos()?, end: diagnostic.end()? })
}

// A compiler diagnostic on a content-mapped file has a range in the mapper's virtual text: as the CLI's diagnostic
// writer does, it is reported at the original range, or on the whole file when it lies in synthesized code. A
// mapper's own diagnostics (they have a source) already carry original ranges.
fn original_range(diagnostic: P<TsDiagnostic>, file: P<SourceFile>) -> Option<Range> {
    let Some(span_map) = file.span_map().filter(|_| diagnostic.source().is_empty()) else {
        return diagnostic_range(diagnostic);
    };
    let (loc, fidelity) = span_map.virtual_to_original_span(diagnostic.loc()?);
    if fidelity == tsrs_spanmap::Fidelity::None {
        return None;
    }
    Some(Range { pos: loc.source_pos()?, end: loc.source_end()? })
}

pub(crate) fn emit(diagnostic: &Diagnostic, writer: &mut dyn Write) -> Result<(), String> {
    protocol::write_message(writer, MessageType::Diagnostic, &diagnostic.to_json())
        .map_err(|e| e.to_string())
}

#[expect(clippy::implicit_hasher, reason = "Public for the integration tests.")]
pub fn find_tsconfig(
    file: &str,
    host: &ConfigHost,
    configs: &mut FxHashMap<tspath::Path, Option<ParsedCommandLine>>,
) -> Option<String> {
    let mut next = compute_config_file_name(file, false, host);
    let mut visited = FxHashSet::default();
    let file_path = tspath::to_path(file, &host.cwd, host.fs.use_case_sensitive_file_names());
    while let Some(config_path) = next {
        let mut queue = VecDeque::from([config_path.clone()]);
        let mut disable_solution_searching = false;
        while let Some(path) = queue.pop_front() {
            if !visited.insert(path.clone()) {
                continue;
            }
            let config_path_key =
                tspath::to_path(&path, &host.cwd, host.fs.use_case_sensitive_file_names());
            let config = configs.entry(config_path_key).or_insert_with(|| {
                let (config, diagnostics) = tsrs_tsoptions::get_parsed_command_line_of_config_file(
                    &path,
                    host.existing_options.as_ref(),
                    None,
                    host,
                    None,
                );
                if !diagnostics.is_empty() {
                    return None;
                }
                config.filter(|config| {
                    config.config_file.is_none_or(|file| file.source_file.diagnostics().is_empty())
                })
            });
            if let Some(config) = config {
                if path == config_path {
                    disable_solution_searching = config
                        .compiler_options()
                        .is_some_and(|options| options.disable_solution_searching.is_true());
                }
                if !config.file_names().is_empty()
                    && (config
                        .compiler_options()
                        .is_none_or(|options| !options.composite.is_true())
                        || config.possibly_matches_file_name(file))
                    && config.file_names_by_path().contains_key(&file_path)
                {
                    return Some(path);
                }
                queue.extend(config.resolved_project_reference_paths().iter().cloned());
            }
        }
        if disable_solution_searching {
            return None;
        }
        next = compute_config_file_name(&config_path, true, host);
    }
    None
}

fn compute_config_file_name(file: &str, skip_directory: bool, host: &ConfigHost) -> Option<String> {
    let mut directory = tspath::get_directory_path(file);
    let mut skip_tsconfig = skip_directory;
    let mut skip_jsconfig = skip_directory && !file.ends_with("/tsconfig.json");
    loop {
        for (name, skip) in [("tsconfig.json", skip_tsconfig), ("jsconfig.json", skip_jsconfig)] {
            if !skip {
                let path = tspath::combine_paths(&directory, &[name]);
                if host.fs.file_exists(&path) {
                    return Some(path);
                }
            }
        }
        if directory.ends_with("/node_modules") {
            return None;
        }
        let parent = tspath::get_directory_path(&directory);
        if parent == directory {
            return None;
        }
        directory = parent;
        skip_tsconfig = false;
        skip_jsconfig = false;
    }
}
