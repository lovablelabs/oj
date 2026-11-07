#![expect(
    clippy::redundant_pub_crate,
    reason = "Only the public API is re-exported; implementation helpers remain crate-private."
)]

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tsrs_ast::Diagnostic;
use tsrs_contentmapper as contentmapper;
use tsrs_core::{CompilerOptions, Locale, P};
use tsrs_vfs::FS;

pub trait System: Sync {
    fn fs(&self) -> Arc<dyn FS>;
    fn default_library_path(&self) -> &str;
    fn get_current_directory(&self) -> &str;
    fn write(&self, text: &str);
    fn flush(&self);
    fn write_output_is_tty(&self) -> bool;
    fn get_environment_variable(&self, name: &str) -> Option<String>;
    // compile.go:31: starts a content mapper process (the production system's is
    // `tsrs_contentmapper::spawn_process`, Go's cmd/tsc/sys.go spawnProcess). A `None` stderr is Go's io.Discard.
    fn spawn(
        &self,
        command: &[String],
        dir: &str,
        stderr: Option<Box<dyn Write + Send>>,
    ) -> Result<super::ReadWriteCloser, String>;
    // Go `sys.ErrorWriter()`, which only the content mapper logger writes to here.
    fn write_error(&self, text: &str) {
        eprint!("{text}");
    }

    fn now(&self) -> Instant;
    fn since_start(&self) -> Duration;

    // Go `sys.Now()` as a wall-clock time (time stamps written by --build).
    fn now_time(&self) -> std::time::SystemTime {
        std::time::SystemTime::now()
    }

    // Go `sys.Now().Format("03:04:05 PM")` (build status lines), in local time.
    fn format_time_now(&self) -> String {
        format_local_time_03_04_05_pm(self.now_time())
    }

    /// tsrs-only: where diagnostics go instead of the text reporters (the WebAssembly build's JSON diagnostics).
    /// When it is set, the reporters pass each diagnostic to it, the error summary prints nothing, and `tsc -b` is
    /// refused. The CLI and the tsctests harness have none.
    fn diagnostic_sink(&self) -> Option<&(dyn Fn(P<Diagnostic>) + Sync)> {
        None
    }
}

/// Uses the command's clock only when compilation statistics were requested.
pub(crate) struct DiagnosticTimer<'a> {
    sys: &'a dyn System,
    start: Option<Instant>,
}

impl<'a> DiagnosticTimer<'a> {
    pub(crate) fn new(sys: &'a dyn System, enabled: bool) -> Self {
        Self { sys, start: enabled.then(|| sys.now()) }
    }

    pub(crate) fn elapsed(self) -> Duration {
        self.start.map_or(Duration::ZERO, |start| self.sys.now() - start)
    }
}

// compile.go:37
fn new_content_mapper_logger(sys: &'static dyn System) -> Option<contentmapper::Logger> {
    if sys.get_environment_variable("TS_CONTENT_MAPPER_DEBUG").unwrap_or_default().is_empty() {
        return None;
    }
    let mu = Mutex::new(());
    Some(Arc::new(move |message: &str| {
        let _guard = mu.lock().unwrap();
        sys.write_error(&format!("{message}\n"));
    }))
}

// NewContentMapperHost creates a content mapper host when content mappers are enabled via the
// --runExternalCode flag, spawning mapper processes through the system's Spawn. It returns
// nil otherwise, in which case no content-mapped files can be loaded. The caller owns the host and must
// Close it when the compilation session ends.
// compile.go:89 (Go binds the host to the command's context, which closes it when the process ends; the callers
// close it explicitly instead. The locale is the option's text: tsrs does not canonicalize BCP 47 tags.)
pub(crate) fn new_content_mapper_host(
    sys: &'static dyn System,
    options: &CompilerOptions,
) -> Option<Arc<dyn contentmapper::Host>> {
    if !options.run_external_code.is_true() {
        return None;
    }
    let diagnostic_locale = Locale(options.locale.clone());
    let spawner: Arc<dyn contentmapper::Spawner> = Arc::new(contentmapper::SpawnerFunc(
        move |command: &[String], dir: &str, stderr: Option<Box<dyn Write + Send>>| {
            sys.spawn(command, dir, stderr)
        },
    ));
    Some(contentmapper::new_host_with_options(
        spawner,
        diagnostic_locale,
        contentmapper::HostOptions { logger: new_content_mapper_logger(sys) },
    ))
}

// Go's `defer contentMapperProject.Close()` (tsc.go:300, 358) and the close of the host, which Go leaves to the
// command's context (it is cancelled when the process ends): the project is closed first, then the host, on every
// way out of the compilation. A host that is still open keeps its mapper processes running.
pub struct ContentMapperCloser {
    pub(crate) host: Option<Arc<dyn contentmapper::Host>>,
    pub(crate) project: Option<Arc<dyn contentmapper::Project>>,
}

impl Drop for ContentMapperCloser {
    fn drop(&mut self) {
        if let Some(project) = &self.project {
            let _ = project.close();
        }
        if let Some(host) = &self.host {
            let _ = host.close();
        }
    }
}

#[cfg(unix)]
fn format_local_time_03_04_05_pm(t: std::time::SystemTime) -> String {
    let secs = t.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as _);
    // SAFETY: `libc::tm` is C integers (and, on some platforms, a nullable `char *`), for which all zeros is valid.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are to live locals; localtime_r reads `secs` and writes only into `tm`.
    unsafe { libc::localtime_r(&raw const secs, &raw mut tm) };
    let hour12 = match tm.tm_hour % 12 {
        0 => 12,
        h => h,
    };
    format!(
        "{:02}:{:02}:{:02} {}",
        hour12,
        tm.tm_min,
        tm.tm_sec,
        if tm.tm_hour < 12 { "AM" } else { "PM" }
    )
}

/// Targets without `localtime_r` (WASI has no time zone): UTC, as Go's js/wasm reports it.
#[cfg(not(unix))]
pub fn format_local_time_03_04_05_pm(t: std::time::SystemTime) -> String {
    let secs = t.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()) % 86_400;
    let (hour, min, sec) = (secs / 3600, secs / 60 % 60, secs % 60);
    let hour12 = match hour % 12 {
        0 => 12,
        h => h,
    };
    format!("{:02}:{:02}:{:02} {}", hour12, min, sec, if hour < 12 { "AM" } else { "PM" })
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(i32)]
pub enum ExitStatus {
    Success = 0,
    DiagnosticsPresentOutputsSkipped = 1,
    DiagnosticsPresentOutputsGenerated = 2,
    InvalidProjectOutputsSkipped = 3,
    ProjectReferenceCycleOutputsSkipped = 4,
    NotImplemented = 5,
}

// Go `tsc.CommandLineTesting`: hooks of the tsctests harness (execute/tsctests); the CLI passes none.
pub trait CommandLineTesting: Sync {
    fn on_list_files_start(&self, _w: &dyn Fn(&str)) {}
    fn on_list_files_end(&self, _w: &dyn Fn(&str)) {}
    fn on_statistics_start(&self, _w: &dyn Fn(&str)) {}
    fn on_statistics_end(&self, _w: &dyn Fn(&str)) {}
    fn on_build_status_report_start(&self, _w: &dyn Fn(&str)) {}
    fn on_build_status_report_end(&self, _w: &dyn Fn(&str)) {}
    // Go `OnWatchStatusReportStart` / `OnWatchStatusReportEnd` (they write to `sys.Writer()`, passed here as `w`).
    fn on_watch_status_report_start(&self, _w: &dyn Fn(&str)) {}
    fn on_watch_status_report_end(&self, _w: &dyn Fn(&str)) {}
    // Go `CommandLineTestingWithWatchBackend.WatchBackend`: the harness's mock instead of fswatch.
    fn watch_backend(&self) -> Option<std::sync::Arc<dyn crate::watchmanager::WatchBackend>> {
        None
    }
    fn on_emitted_files(
        &self,
        _result: Option<&tsrs_compiler::EmitResult>,
        _m_times_cache: Option<&MTimesCache>,
    ) {
    }
    fn on_program(&self, _program: P<tsrs_incremental::Program>) {}
    // tsrs-only: the watch computes placeholder signatures after each cycle (watcher.rs `start_signature_job`), on the
    // cycle's thread, before the next edit. Off by default, as Go has no such step.
    fn run_signature_job(&self) -> bool {
        false
    }
    // Go `GetTrace(w, locale)`: the trace function for a program's compiler host, writing to `w`.
    fn get_trace(&'static self, w: SyncWriter, is_sys_writer: bool) -> Box<tsrs_compiler::TraceFn>;
}

pub type SyncWriter = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

// The build host's mtime cache (Go `*collections.SyncMap[tspath.Path, time.Time]`, zero time = None).
pub type MTimesCache = std::sync::Arc<
    tsrs_core::collections::SyncMap<tsrs_core::tspath::Path, Option<std::time::SystemTime>>,
>;

// emit.go:21 GetTraceWithWriterFromSys
pub(crate) fn get_trace_with_writer_from_sys(
    w: SyncWriter,
    is_sys_writer: bool,
    testing: Option<&'static dyn CommandLineTesting>,
) -> Box<tsrs_compiler::TraceFn> {
    match testing {
        None => Box::new(
            move |msg: &'static tsrs_diagnostics::Message, args: &[&dyn std::fmt::Display]| {
                w(&format!("{}\n", msg.localize(args)));
            },
        ),
        Some(testing) => testing.get_trace(w, is_sys_writer),
    }
}

// compile.go:61
pub trait Watcher {
    fn do_cycle(&mut self);
}

pub struct CommandLineResult {
    pub status: ExitStatus,
    // Go `Watcher`: set by watch mode under the test harness, which drives the cycles itself.
    pub watcher: Option<Box<dyn Watcher>>,
}

#[derive(Default, Clone)]
pub struct CompileTimes {
    pub(crate) config_time: Duration,
    pub(crate) parse_time: Duration,
    pub(crate) content_mapper_times: contentmapper::Timings,
    pub(crate) bind_time: Duration,
    pub(crate) check_time: Duration,
    pub(crate) total_time: Duration,
    pub(crate) emit_time: Duration,
    pub(crate) build_info_read_time: Duration,
    pub(crate) changes_compute_time: Duration,
}

pub(crate) struct CompileAndEmitResult {
    pub(crate) diagnostics: Vec<P<Diagnostic>>,
    pub(crate) emit_skipped: bool,
    // Go `EmitResult.EmittedFiles`.
    pub(crate) emitted_files: Vec<String>,
    pub(crate) status: ExitStatus,
    pub(crate) times: CompileTimes,
}
