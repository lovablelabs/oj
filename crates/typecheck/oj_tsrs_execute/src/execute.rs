// Port of Go's execute/tsc.go (no --init, --showConfig or --generateTrace; --help prints a short usage instead of
// tsc's option list).

use std::sync::Arc;

use tsrs_ast::new_compiler_diagnostic;
use tsrs_compiler::{ProgramOptions, new_cached_fs_compiler_host, new_program};
use tsrs_core::P;
use tsrs_core::tspath;
use tsrs_diagnostics as diagnostics;
use tsrs_tsoptions::{self as tsoptions, CompilerOptionsValue, ParseConfigHost, ParsedCommandLine};
use tsrs_vfs::FS;

use crate::tsc::{
    self, CommandLineResult, CompileTimes, EmitInput, ExitStatus, ExtendedConfigCache, System,
    create_diagnostic_formatter, create_diagnostic_reporter, create_report_error_summary,
    emit_and_report_statistics,
};

// The system doubles as the config-parsing host (Go's tsc.System satisfies tsoptions.ParseConfigHost).
pub struct SysParseConfigHost {
    sys: &'static dyn System,
    fs: Arc<dyn FS>,
}

/// The config parse host for `sys` (leaked: tsoptions keeps it for the parsed command line's lifetime).
pub fn parse_config_host(sys: &'static dyn System) -> &'static SysParseConfigHost {
    Box::leak(Box::new(SysParseConfigHost { sys, fs: sys.fs() }))
}

impl ParseConfigHost for SysParseConfigHost {
    fn fs(&self) -> &dyn FS {
        &*self.fs
    }

    fn get_current_directory(&self) -> &str {
        self.sys.get_current_directory()
    }
}

fn not_supported(sys: &dyn System, what: &str) -> CommandLineResult {
    sys.write(&format!(
        "error: {what} is not supported by tsrs.
"
    ));
    CommandLineResult { watcher: None, status: ExitStatus::NotImplemented }
}

pub fn command_line(sys: &'static dyn System, command_line_args: Vec<String>) -> CommandLineResult {
    command_line_with_testing(sys, command_line_args, None)
}

// tsc.go:56 CommandLine(ctx, sys, commandLineArgs, testing)
pub fn command_line_with_testing(
    sys: &'static dyn System,
    command_line_args: Vec<String>,
    testing: Option<&'static dyn tsc::CommandLineTesting>,
) -> CommandLineResult {
    if let Some(first) = command_line_args.first() {
        match first.to_lowercase().as_str() {
            "-b" | "--b" | "-build" | "--build" => {
                let host = parse_config_host(sys);
                let mut command = tsoptions::parse_build_command_line(&command_line_args, host);
                configure_performance(&command.raw);
                if tsrs_core::NO_THREADS {
                    command.compiler_options.single_threaded = tsrs_core::Tristate::True;
                }
                return tsc_build_compilation(sys, P::new(command), testing);
            }
            _ => {}
        }
    }

    let mut args = command_line_args;
    // tsrs-only: `--noLazyMembers` turns off the port of the lazy member resolution PRs (tsrs_core::lazymembers).
    for (flag, on) in [("--lazyMembers", true), ("--noLazyMembers", false)] {
        if args.iter().any(|a| a.eq_ignore_ascii_case(flag)) {
            tsrs_core::lazymembers::set_from_cli(on);
            args.retain(|a| !a.eq_ignore_ascii_case(flag));
        }
    }
    // tsrs-only: `--checkerAssignment <locality|go|random:<seed>|file:<path>>` picks how files are assigned to checkers
    // (tsrs_compiler checkerpool.rs; `go` is Go's FENNEL assignment and Go's check history, tsrs_core::compat).
    if let Some(pos) = args.iter().position(|a| a.eq_ignore_ascii_case("--checkerAssignment")) {
        let name = args.get(pos + 1).cloned().unwrap_or_default();
        if !tsrs_compiler::set_checker_assignment_from_cli(&name) {
            sys.write(&format!("error: unknown --checkerAssignment {name:?} (expected locality, go, random:<seed> or file:<path>).\n"));
            return CommandLineResult {
                watcher: None,
                status: ExitStatus::DiagnosticsPresentOutputsSkipped,
            };
        }
        args.drain(pos..(pos + 2).min(args.len()));
    }
    // tsrs-only, opt-in: `--checkerCostCache <file>` balances the checkers on per-file check times measured by
    // the previous run that used the same file, and writes this run's times to it (checkerpool.rs).
    if let Some(pos) = args.iter().position(|a| a.eq_ignore_ascii_case("--checkerCostCache")) {
        let Some(path) = args.get(pos + 1).cloned().filter(|p| !p.starts_with('-')) else {
            sys.write("error: --checkerCostCache expects a file path.\n");
            return CommandLineResult {
                watcher: None,
                status: ExitStatus::DiagnosticsPresentOutputsSkipped,
            };
        };
        if tsrs_core::NO_THREADS {
            sys.write("error: --checkerCostCache is not supported by the WebAssembly build.\n");
            return CommandLineResult {
                watcher: None,
                status: ExitStatus::DiagnosticsPresentOutputsSkipped,
            };
        }
        tsrs_compiler::set_checker_cost_cache_from_cli(&path);
        args.drain(pos..pos + 2);
    }
    // tsrs-only, opt-in: `--maxMemory <size>` (e.g. `12G`) is a target for the process's memory in `--noEmit` checks:
    // above it, the type-check pass retires its largest checker and checks the rest of that checker's files with a
    // fresh one, trading CPU for memory (checkerpool.rs; notes/mem-recycle-checkers.md).
    if let Some(pos) = args.iter().position(|a| a.eq_ignore_ascii_case("--maxMemory")) {
        let Some(bytes) = args.get(pos + 1).and_then(|v| tsrs_compiler::parse_memory_size(v))
        else {
            sys.write("error: --maxMemory expects a size such as 12G, 12000M or 12000 (MiB).\n");
            return CommandLineResult {
                watcher: None,
                status: ExitStatus::DiagnosticsPresentOutputsSkipped,
            };
        };
        tsrs_compiler::set_max_memory_from_cli(bytes);
        args.drain(pos..pos + 2);
    }
    let host = parse_config_host(sys);
    let mut command = tsoptions::parse_command_line(&args, host);
    configure_performance(&command.raw);
    if tsrs_core::NO_THREADS {
        force_single_threaded(&mut command);
    }
    tsc_compilation(sys, host, P::new(command), testing)
}

fn configure_performance(raw: &CompilerOptionsValue) {
    if let CompilerOptionsValue::Object(options) = raw
        && let Some(fast) = options.get("fast")
    {
        tsrs_core::fast::set_enabled(matches!(fast, CompilerOptionsValue::Bool(true)));
    }
}

/// `tsrs_core::NO_THREADS`: `--singleThreaded`, set on the parsed options so every reader agrees (a config file
/// cannot turn it off: command-line options win over it).
fn force_single_threaded(command: &mut ParsedCommandLine) {
    if let Some(options) = command.parsed_config.compiler_options {
        let mut options = (*options).clone();
        options.single_threaded = tsrs_core::Tristate::True;
        command.parsed_config.compiler_options = Some(P::new(options));
    }
}

fn tsc_compilation(
    sys: &'static dyn System,
    host: &'static SysParseConfigHost,
    command_line: P<ParsedCommandLine>,
    testing: Option<&'static dyn tsc::CommandLineTesting>,
) -> CommandLineResult {
    let mut config_file_name = String::new();
    let command_line_options = command_line.compiler_options().unwrap();
    let report_diagnostic = create_diagnostic_reporter(sys, Some(&command_line_options));
    let mut format_diagnostic = create_diagnostic_formatter(sys, Some(&command_line_options));

    if !command_line.errors.is_empty() {
        for &e in &command_line.errors {
            report_diagnostic(e);
        }
        return CommandLineResult {
            watcher: None,
            status: ExitStatus::DiagnosticsPresentOutputsSkipped,
        };
    }

    if command_line_options.init.is_true() {
        return not_supported(sys, "--init");
    }

    if command_line_options.version.is_true() {
        print_version(sys);
        return CommandLineResult { watcher: None, status: ExitStatus::Success };
    }

    if command_line_options.help.is_true() || command_line_options.all.is_true() {
        print_version(sys);
        sys.write("Usage: tsrs [-p <project>] [options] [files...]\n  Compiles like `tsc`. See `tsc --help` for options.\n  --fast  Enable tsrs performance modes that may differ from pinned TypeScript behavior.\n");
        return CommandLineResult { watcher: None, status: ExitStatus::Success };
    }

    if command_line_options.watch.is_true() && command_line_options.list_files_only.is_true() {
        report_diagnostic(new_compiler_diagnostic(
            &diagnostics::OPTIONS_0_AND_1_CANNOT_BE_COMBINED,
            &[&"watch", &"listFilesOnly"],
        ));
        return CommandLineResult {
            watcher: None,
            status: ExitStatus::DiagnosticsPresentOutputsSkipped,
        };
    }

    let fs = sys.fs();
    if !command_line_options.project.is_empty() {
        if !command_line.file_names().is_empty() {
            report_diagnostic(new_compiler_diagnostic(
                &diagnostics::OPTION_PROJECT_CANNOT_BE_MIXED_WITH_SOURCE_FILES_ON_A_COMMAND_LINE,
                &[],
            ));
            return CommandLineResult {
                watcher: None,
                status: ExitStatus::DiagnosticsPresentOutputsSkipped,
            };
        }

        let file_or_directory = tspath::normalize_path(&command_line_options.project);
        if fs.directory_exists(&file_or_directory) {
            config_file_name = tspath::combine_paths(&file_or_directory, &["tsconfig.json"]);
            if !fs.file_exists(&config_file_name) {
                report_diagnostic(new_compiler_diagnostic(
                    &diagnostics::CANNOT_FIND_A_TSCONFIG_JSON_FILE_AT_THE_CURRENT_DIRECTORY_COLON_0,
                    &[&config_file_name],
                ));
                return CommandLineResult {
                    watcher: None,
                    status: ExitStatus::DiagnosticsPresentOutputsSkipped,
                };
            }
        } else {
            config_file_name.clone_from(&file_or_directory);
            if !fs.file_exists(&config_file_name) {
                report_diagnostic(new_compiler_diagnostic(
                    &diagnostics::THE_SPECIFIED_PATH_DOES_NOT_EXIST_COLON_0,
                    &[&file_or_directory],
                ));
                return CommandLineResult {
                    watcher: None,
                    status: ExitStatus::DiagnosticsPresentOutputsSkipped,
                };
            }
        }
    } else if !command_line_options.ignore_config.is_true() || command_line.file_names().is_empty()
    {
        let search_path = tspath::normalize_path(sys.get_current_directory());
        config_file_name = find_config_file(&search_path, |f| fs.file_exists(f), "tsconfig.json");
        if !command_line.file_names().is_empty() {
            if !config_file_name.is_empty() {
                // Error to not specify config file
                report_diagnostic(new_compiler_diagnostic(
                    &diagnostics::X_TSCONFIG_JSON_IS_PRESENT_BUT_WILL_NOT_BE_LOADED_IF_FILES_ARE_SPECIFIED_ON_COMMANDLINE_USE_IGNORE_CONFIG_TO_SKIP_THIS_ERROR,
                    &[],
                ));
                return CommandLineResult {
                    watcher: None,
                    status: ExitStatus::DiagnosticsPresentOutputsSkipped,
                };
            }
        } else if config_file_name.is_empty() {
            if command_line_options.show_config.is_true() {
                report_diagnostic(new_compiler_diagnostic(
                    &diagnostics::CANNOT_FIND_A_TSCONFIG_JSON_FILE_AT_THE_CURRENT_DIRECTORY_COLON_0,
                    &[&tspath::normalize_path(sys.get_current_directory())],
                ));
            } else {
                print_version(sys);
                sys.write("Usage: tsrs [-p <project>] [options] [files...]\n");
            }
            return CommandLineResult {
                watcher: None,
                status: ExitStatus::DiagnosticsPresentOutputsSkipped,
            };
        }
    }

    // !!! convert to options with absolute paths is usually done here, but for ease of implementation, it's done in `tsoptions.ParseCommandLine()`
    let compiler_options_from_command_line = command_line_options;
    let mut config_for_compilation = command_line;
    let extended_config_cache: Arc<ExtendedConfigCache> = Arc::new(ExtendedConfigCache::default());
    let mut compile_times = CompileTimes::default();
    let mut report_diagnostic = report_diagnostic;
    // Wrap command line options in a "compilerOptions" key to match tsconfig.json structure
    let command_line_raw = match &command_line.raw {
        CompilerOptionsValue::Object(_) => {
            let mut wrapped = tsrs_core::collections::OrderedMap::default();
            wrapped.insert("compilerOptions".to_string(), command_line.raw.clone());
            Some(CompilerOptionsValue::Object(wrapped))
        }
        _ => None,
    };
    if !config_file_name.is_empty() {
        let config_start = tsc::DiagnosticTimer::new(
            sys,
            compiler_options_from_command_line.diagnostics_enabled(),
        );
        // tsrs-only: parsed on the compiler's worker pool, so that the include glob's parallel listing prefetch runs on
        // the pool program construction uses next instead of starting rayon's global pool (one thread per vCPU, which
        // nothing else in a run needs).
        let (config_parse_result, errors) = tsrs_compiler::worker_pool().install(|| {
            tsoptions::get_parsed_command_line_of_config_file(
                &config_file_name,
                Some(&compiler_options_from_command_line),
                command_line_raw.as_ref(),
                host,
                Some(&*extended_config_cache),
            )
        });
        compile_times.config_time = config_start.elapsed();
        if !errors.is_empty() {
            // these are unrecoverable errors--exit to report them as diagnostics
            for e in errors {
                report_diagnostic(e);
            }
            return CommandLineResult {
                watcher: None,
                status: ExitStatus::DiagnosticsPresentOutputsGenerated,
            };
        }
        config_for_compilation = P::new(config_parse_result.unwrap());
        // Updater to reflect pretty
        report_diagnostic =
            create_diagnostic_reporter(sys, Some(&command_line.compiler_options().unwrap()));
        format_diagnostic =
            create_diagnostic_formatter(sys, Some(&command_line.compiler_options().unwrap()));
    }

    let config_options = config_for_compilation.compiler_options().unwrap();
    let report_error_summary = create_report_error_summary(sys, &config_options);
    if compiler_options_from_command_line.show_config.is_true() {
        return not_supported(sys, "--showConfig");
    }
    if config_options.watch.is_true() {
        if testing.is_none() && !tsrs_fswatch::default().available() {
            return not_supported(sys, "watch mode (--watch) on this platform");
        }
        let backend = testing.and_then(crate::tsc::CommandLineTesting::watch_backend);
        let watcher = crate::watcher::run_watcher(
            sys,
            config_for_compilation,
            compiler_options_from_command_line,
            command_line_raw,
            report_diagnostic,
            report_error_summary,
            testing,
            backend,
        );
        return CommandLineResult {
            status: ExitStatus::Success,
            watcher: watcher.map(|w| w as Box<dyn tsc::Watcher>),
        };
    }
    // tsc.go:245
    if config_for_compilation.compiler_options().unwrap().is_incremental() {
        return perform_incremental_compilation(
            sys,
            config_for_compilation,
            &report_diagnostic,
            format_diagnostic.as_ref(),
            &report_error_summary,
            extended_config_cache,
            compile_times,
            testing,
        );
    }
    perform_compilation(
        sys,
        config_for_compilation,
        &report_diagnostic,
        format_diagnostic.as_ref(),
        &report_error_summary,
        extended_config_cache,
        compile_times,
        testing,
    )
}

fn find_config_file(
    search_path: &str,
    file_exists: impl Fn(&str) -> bool,
    config_name: &str,
) -> String {
    tspath::for_each_ancestor_directory(search_path, |ancestor| {
        let full_config_name = tspath::combine_paths(ancestor, &[config_name]);
        if file_exists(&full_config_name) { Some(full_config_name) } else { None }
    })
    .unwrap_or_default()
}

// tsc's "Version X" first (scripts grep for it), then the tsrs release and the TypeScript commit it ports.
fn print_version(sys: &dyn System) {
    sys.write(&format!(
        "{} (tsrs {}, microsoft/TypeScript@{})\n",
        diagnostics::VERSION_0.localize(&[&tsrs_core::version()]),
        env!("TSRS_RELEASE_VERSION"),
        env!("TSRS_TYPESCRIPT_COMMIT"),
    ));
}

// tsc.go:28 startTracingIfNeeded. tsrs has no trace writer (Go's tracing package and the checker's type tracer), so
// this is Go's branch for a trace that cannot be started: a warning, then the compilation runs untraced.
fn start_tracing_if_needed(sys: &dyn System, config: &ParsedCommandLine) {
    if config.compiler_options().unwrap().generate_trace.is_empty() {
        return;
    }
    sys.write("Warning: Failed to start tracing: --generateTrace is not supported by tsrs\n");
}

fn perform_compilation(
    sys: &'static dyn System,
    config: P<ParsedCommandLine>,
    report_diagnostic: &tsc::DiagnosticReporter,
    format_diagnostic: Option<&tsc::DiagnosticFormatter>,
    report_error_summary: &tsc::DiagnosticsReporter,
    extended_config_cache: Arc<ExtendedConfigCache>,
    mut compile_times: CompileTimes,
    testing: Option<&'static dyn tsc::CommandLineTesting>,
) -> CommandLineResult {
    let content_mapper_host =
        tsc::new_content_mapper_host(sys, &config.compiler_options().unwrap());
    let content_mapper_project = get_content_mapper_project(content_mapper_host.as_ref(), config);
    let _close_content_mappers = tsc::ContentMapperCloser {
        host: content_mapper_host.clone(),
        project: content_mapper_project.clone(),
    };
    let host = new_cached_fs_compiler_host(
        sys.get_current_directory(),
        sys.fs(),
        sys.default_library_path(),
        Some(extended_config_cache),
        Some(get_trace_from_sys(sys, testing)),
        content_mapper_project,
    );

    // tsrs-only (notes/mem-free-leaf-files.md): a `--noEmit` check frees the tree of each file that nothing else
    // refers to once it is checked. Not when anything reads the trees after the check pass: declaration
    // diagnostics (`declaration`, `composite`), `--explainFiles` (the import nodes of each file), the tsctests harness,
    // or Go's check history (`--checkerAssignment go`, a per-name cache that keeps another file's declarations).
    let options = config.compiler_options().unwrap();
    let leaf_freeing_allowed = testing.is_none()
        && options.no_emit.is_true()
        && !options.get_emit_declarations()
        && !options.explain_files.is_true()
        && !tsrs_core::compat::go_compatible_history();
    let leaf_settings = if leaf_freeing_allowed {
        tsrs_compiler::leaf_settings_from_env(tsrs_compiler::checker_count_upper_bound(
            &options, false,
        ))
    } else {
        tsrs_compiler::LeafSettings::default()
    };
    if leaf_settings.mode != tsrs_compiler::LeafMode::Off {
        tsrs_compiler::enable_file_regions(leaf_settings, sys.get_current_directory());
    }
    // tsrs-only (notes/mem-lazy-dts-members.md): when no declaration file is type-checked, the member lists of
    // interfaces, classes and type literals in declaration files are parsed and bound on first use.
    if testing.is_none() && declaration_files_unchecked(config) && tsrs_compiler::lazy_dts_allowed()
    {
        tsrs_compiler::enable_lazy_dts();
    }
    let mut program_options = ProgramOptions::new(config, host);
    program_options.leaf_files = leaf_settings.mode;
    program_options.checker_recycling = leaf_freeing_allowed;

    start_tracing_if_needed(sys, &config);
    let parse_start =
        tsc::DiagnosticTimer::new(sys, config.compiler_options().unwrap().diagnostics_enabled());
    let program = new_program(program_options);
    compile_times.parse_time = parse_start.elapsed();
    if let Some(content_mapper_host) = &content_mapper_host {
        compile_times.content_mapper_times = content_mapper_host.timings();
    }
    let (result, _) = emit_and_report_statistics(&EmitInput {
        sys,
        program,
        config,
        report_diagnostic,
        format_diagnostic,
        report_error_summary,
        compile_times,
        incremental: None,
        writer: None,
        write_file: None,
        testing,
        testing_m_times_cache: None,
    });
    if let Some(line) = tsrs_compiler::leaf_stats_report() {
        eprint!("{line}");
    }

    CommandLineResult { watcher: None, status: result.status }
}

// tsc.go:308
fn perform_incremental_compilation(
    sys: &'static dyn System,
    config: P<ParsedCommandLine>,
    report_diagnostic: &tsc::DiagnosticReporter,
    format_diagnostic: Option<&tsc::DiagnosticFormatter>,
    report_error_summary: &tsc::DiagnosticsReporter,
    extended_config_cache: Arc<ExtendedConfigCache>,
    mut compile_times: CompileTimes,
    testing: Option<&'static dyn tsc::CommandLineTesting>,
) -> CommandLineResult {
    let content_mapper_host =
        tsc::new_content_mapper_host(sys, &config.compiler_options().unwrap());
    let content_mapper_project = get_content_mapper_project(content_mapper_host.as_ref(), config);
    let _close_content_mappers = tsc::ContentMapperCloser {
        host: content_mapper_host.clone(),
        project: content_mapper_project.clone(),
    };
    let host = new_cached_fs_compiler_host(
        sys.get_current_directory(),
        sys.fs(),
        sys.default_library_path(),
        Some(extended_config_cache),
        Some(get_trace_from_sys(sys, testing)),
        content_mapper_project,
    );
    // tsrs-only (notes/mem-lazy-dts-members.md): as in `perform_compilation`. The builder uses declaration files'
    // versions as their signatures; its tree readers force any member lists they visit through the usual accessors.
    if testing.is_none() && declaration_files_unchecked(config) && tsrs_compiler::lazy_dts_allowed()
    {
        tsrs_compiler::enable_lazy_dts();
    }
    // Go reads the old program and then builds the new one. The two are independent (the read only uses the
    // config and the host's file system, both thread-safe), so the read runs on its own thread while the program
    // is built; "BuildInfo read time" is the read's own duration. `--singleThreaded` keeps Go's order.
    let read_build_info = || {
        let start = tsc::DiagnosticTimer::new(
            sys,
            config.compiler_options().unwrap().diagnostics_enabled(),
        );
        let old_program = tsrs_incremental::read_build_info_program(
            config,
            &*tsrs_incremental::new_build_info_reader(Arc::clone(&host)),
            &*host,
        );
        (old_program, start.elapsed())
    };
    let build_program = || {
        let start = tsc::DiagnosticTimer::new(
            sys,
            config.compiler_options().unwrap().diagnostics_enabled(),
        );
        let program = new_program(ProgramOptions::new(config, Arc::clone(&host)));
        (program, start.elapsed())
    };
    // Go starts tracing between the build info read and the program; nothing in between writes output.
    start_tracing_if_needed(sys, &config);
    let ((old_program, build_info_read_time), (program, parse_time)) =
        if config.compiler_options().unwrap().single_threaded.is_true() {
            let read = read_build_info();
            (read, build_program())
        } else {
            // Rows print in first-recorded order; keep the read's rows ahead of the program's.
            if config.compiler_options().unwrap().diagnostics_enabled() {
                tsrs_incremental::register_build_info_read_phases();
            }
            std::thread::scope(|scope| {
                let read = scope.spawn(read_build_info);
                let program = build_program();
                (read.join().unwrap(), program)
            })
        };
    compile_times.build_info_read_time = build_info_read_time;
    compile_times.parse_time = parse_time;
    let changes_compute_start =
        tsc::DiagnosticTimer::new(sys, config.compiler_options().unwrap().diagnostics_enabled());
    let incremental_program = tsrs_incremental::new_program(
        program,
        old_program,
        tsrs_incremental::create_host(host),
        Some(std::time::Instant::now),
        testing.is_some(),
    );
    compile_times.changes_compute_time = changes_compute_start.elapsed();
    if let Some(content_mapper_host) = &content_mapper_host {
        compile_times.content_mapper_times = content_mapper_host.timings();
    }
    let (result, _) = emit_and_report_statistics(&EmitInput {
        sys,
        program: incremental_program.get_program(),
        config,
        report_diagnostic,
        format_diagnostic,
        report_error_summary,
        compile_times,
        incremental: Some(incremental_program),
        writer: None,
        write_file: None,
        testing,
        testing_m_times_cache: None,
    });

    if let Some(testing) = testing {
        testing.on_program(incremental_program);
    }
    CommandLineResult { watcher: None, status: result.status }
}

// tsrs-only (notes/mem-lazy-dts-members.md): no declaration file of this program is type-checked.
pub(crate) fn declaration_files_unchecked(config: P<ParsedCommandLine>) -> bool {
    let options = config.compiler_options().unwrap();
    options.skip_lib_check.is_true() || options.no_check.is_true()
}

// tsc.go:394
pub(crate) fn get_content_mapper_project(
    host: Option<&Arc<dyn tsrs_contentmapper::Host>>,
    config: P<ParsedCommandLine>,
) -> Option<Arc<dyn tsrs_contentmapper::Project>> {
    let host = host?;
    // References into the command line's own list (an arena object): the host keys the project by their addresses.
    let config: &'static ParsedCommandLine = config.get();
    if config.content_mappers().is_empty() {
        return None;
    }
    host.project(tsrs_contentmapper::ProjectSpec {
        config_file_name: config.config_name().to_string(),
        mappers: config.content_mappers().iter().collect(),
        compiler_options: config.compiler_options(),
    })
}

// tsc.go:93
fn tsc_build_compilation(
    sys: &'static dyn System,
    build_command: P<tsoptions::ParsedBuildCommandLine>,
    testing: Option<&'static dyn tsc::CommandLineTesting>,
) -> CommandLineResult {
    // tsrs-only: build mode has its own reporters (task output buffers, build status), which a sink does not cover.
    if sys.diagnostic_sink().is_some() {
        sys.write("error: diagnostics as JSON are not supported with --build\n");
        return CommandLineResult {
            watcher: None,
            status: ExitStatus::DiagnosticsPresentOutputsSkipped,
        };
    }
    let report_diagnostic = create_diagnostic_reporter(sys, Some(&build_command.compiler_options));

    if !build_command.errors.is_empty() {
        for &err in &build_command.errors {
            report_diagnostic(err);
        }
        return CommandLineResult {
            watcher: None,
            status: ExitStatus::DiagnosticsPresentOutputsSkipped,
        };
    }

    if build_command.compiler_options.help.is_true() {
        print_version(sys);
        sys.write("Usage: tsrs -b [projects...] [options]\n  See `tsc -b --help` for options.\n  --fast  Enable tsrs performance modes that may differ from pinned TypeScript behavior.\n");
        return CommandLineResult { watcher: None, status: ExitStatus::Success };
    }
    // tsrs-only: no file watching backend on this platform (Windows; Go has ReadDirectoryChangesW).
    if build_command.compiler_options.watch.is_true()
        && testing.is_none()
        && !tsrs_fswatch::default().available()
    {
        return not_supported(sys, "watch mode (--watch) on this platform");
    }
    let orchestrator = crate::build::new_orchestrator(crate::build::Options {
        sys,
        command: build_command,
        testing,
    });
    orchestrator.start()
}

// tsc.go getTraceFromSys / tsc.GetTraceWithWriterFromSys
pub(crate) fn get_trace_from_sys(
    sys: &'static dyn System,
    testing: Option<&'static dyn tsc::CommandLineTesting>,
) -> Box<tsrs_compiler::TraceFn> {
    tsc::get_trace_with_writer_from_sys(
        std::sync::Arc::new(move |t: &str| sys.write(t)),
        true,
        testing,
    )
}
