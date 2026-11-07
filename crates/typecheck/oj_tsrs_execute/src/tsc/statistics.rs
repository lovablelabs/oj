use std::fmt::Write as _;
use std::time::Duration;

use super::{CompileTimes, EmitInput};

struct TableRow {
    name: String,
    value: String,
}

#[derive(Default)]
struct Table {
    rows: Vec<TableRow>,
}

impl Table {
    fn add(&mut self, name: &str, value: &dyn std::fmt::Display) {
        self.rows.push(TableRow { name: name.to_string(), value: value.to_string() });
    }

    fn add_duration(&mut self, name: &str, d: Duration) {
        self.add(name, &format_duration(d));
    }

    fn print(&self, w: &dyn Fn(&str)) {
        let mut name_width = 0;
        let mut value_width = 0;
        for r in &self.rows {
            name_width = name_width.max(r.name.len());
            value_width = value_width.max(r.value.len());
        }

        let mut out = String::new();
        for r in &self.rows {
            // Go: "%-*s %*s\n"
            let name = format!("{}:", r.name);
            let _ = writeln!(
                out,
                "{:<nw$} {:>vw$}",
                name,
                r.value,
                nw = name_width + 1,
                vw = value_width
            );
        }
        w(&out);
    }
}

fn format_duration(d: Duration) -> String {
    format!("{:.3}s", d.as_secs_f64())
}

#[derive(Clone, Default)]
pub struct Statistics {
    is_aggregate: bool,
    pub projects: usize,
    pub projects_built: usize,
    pub timestamp_updates: usize,
    files: usize,
    lines: usize,
    identifiers: usize,
    symbols: usize,
    types: usize,
    instantiations: usize,
    memory_used: u64,
    memory_allocs: u64,
    compile_times: CompileTimes,
    lazy_member_stats: Option<tsrs_core::lazymembers::LazyMemberStats>,
}

// Go reports runtime.MemStats (live heap and malloc count). There is no GC heap here; the resident
// set size of the process stands in for "Memory used", and allocation counts are not tracked.
// The number `ps -o rss=` prints, read without starting `ps` (about 2 ms and a process spawn at the end of every
// `--extendedDiagnostics` run): the resident pages of /proc/self/statm, which is where Linux `ps` reads it.
#[cfg(target_os = "linux")]
fn memory_used_bytes() -> u64 {
    let statm = tsrs_core::utf8::read_to_string("/proc/self/statm").unwrap_or_default();
    let pages = statm.split_whitespace().nth(1).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
    // SAFETY: sysconf has no preconditions.
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    pages * page_size.max(0) as u64
}

// macOS `ps` reports the task's resident size from proc_pidinfo(PROC_PIDTASKINFO), in KiB.
#[cfg(target_os = "macos")]
fn memory_used_bytes() -> u64 {
    // SAFETY: proc_taskinfo is plain integers, for which all zeroes is a valid value.
    let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
    // SAFETY: `info` is a writable proc_taskinfo of `size` bytes, as PROC_PIDTASKINFO requires.
    let n = unsafe {
        libc::proc_pidinfo(
            std::process::id() as libc::c_int,
            libc::PROC_PIDTASKINFO,
            0,
            std::ptr::addr_of_mut!(info).cast(),
            size,
        )
    };
    if n != size {
        return 0;
    }
    info.pti_resident_size / 1024 * 1024
}

/// WebAssembly: the size of linear memory, which never shrinks, so it is the peak.
#[cfg(target_family = "wasm")]
fn memory_used_bytes() -> u64 {
    core::arch::wasm32::memory_size(0) as u64 * 65536
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_family = "wasm")))]
fn memory_used_bytes() -> u64 {
    let pid = std::process::id().to_string();
    std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid])
        .output()
        .ok()
        .and_then(|o| tsrs_core::utf8::into_string(o.stdout).ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|kb| kb * 1024)
        .unwrap_or(0)
}

#[expect(
    clippy::redundant_pub_crate,
    reason = "Only the public API is re-exported; implementation helpers remain crate-private."
)]
pub(crate) fn statistics_from_program(input: &EmitInput, times: &CompileTimes) -> Statistics {
    let program = input.program;
    Statistics {
        is_aggregate: false,
        projects: 0,
        projects_built: 0,
        timestamp_updates: 0,
        files: program.source_files().len(),
        lines: program.line_count(),
        identifiers: program.identifier_count(),
        symbols: program.symbol_count(),
        types: program.type_count(),
        instantiations: program.instantiation_count(),
        memory_used: memory_used_bytes(),
        memory_allocs: 0,
        compile_times: times.clone(),
        lazy_member_stats: tsrs_core::lazymembers::enabled().then(|| program.lazy_member_stats()),
    }
}

impl Statistics {
    pub(crate) fn report(&self, w: &dyn Fn(&str), testing: Option<&dyn super::CommandLineTesting>) {
        if let Some(testing) = testing {
            testing.on_statistics_start(w);
        }
        self.report_worker(w);
        if let Some(testing) = testing {
            testing.on_statistics_end(w);
        }
    }

    fn report_worker(&self, w: &dyn Fn(&str)) {
        let mut table = Table::default();
        let prefix = if self.is_aggregate { "Aggregate " } else { "" };

        if self.is_aggregate {
            table.add("Projects in scope", &self.projects);
            table.add("Projects built", &self.projects_built);
            table.add("Timestamps only updates", &self.timestamp_updates);
        }
        table.add(&format!("{prefix}Files"), &self.files);
        table.add(&format!("{prefix}Lines"), &self.lines);
        table.add(&format!("{prefix}Identifiers"), &self.identifiers);
        table.add(&format!("{prefix}Symbols"), &self.symbols);
        table.add(&format!("{prefix}Types"), &self.types);
        table.add(&format!("{prefix}Instantiations"), &self.instantiations);
        table.add(&format!("{prefix}Memory used"), &format!("{}K", self.memory_used / 1024));
        table.add(&format!("{prefix}Memory allocs"), &self.memory_allocs);
        if !self.compile_times.config_time.is_zero() {
            table.add_duration(&format!("{prefix}Config time"), self.compile_times.config_time);
        }
        if !self.compile_times.build_info_read_time.is_zero() {
            table.add_duration(
                &format!("{prefix}BuildInfo read time"),
                self.compile_times.build_info_read_time,
            );
        }
        table.add_duration(&format!("{prefix}Parse time"), self.compile_times.parse_time);
        if !self.compile_times.bind_time.is_zero() {
            table.add_duration(&format!("{prefix}Bind time"), self.compile_times.bind_time);
        }
        if !self.compile_times.check_time.is_zero() {
            table.add_duration(&format!("{prefix}Check time"), self.compile_times.check_time);
        }
        if !self.compile_times.emit_time.is_zero() {
            table.add_duration(&format!("{prefix}Emit time"), self.compile_times.emit_time);
        }
        if !self.compile_times.changes_compute_time.is_zero() {
            table.add_duration(
                &format!("{prefix}Changes compute time"),
                self.compile_times.changes_compute_time,
            );
        }
        self.add_content_mapper_statistics(&mut table, prefix);
        table.add_duration(&format!("{prefix}Total time"), self.compile_times.total_time);
        table.print(w);
        if self.is_aggregate {
            return;
        }
        // tsrs-only sub-phases (tsrs_core::phases), in the order they first ran.
        let mut table = Table::default();
        for (name, value) in tsrs_core::phases::snapshot() {
            match value {
                tsrs_core::phases::PhaseValue::Time(d) => table.add_duration(name, d),
                tsrs_core::phases::PhaseValue::Count(n) => table.add(name, &n),
            }
        }
        table.print(w);
        if let Some(stats) = &self.lazy_member_stats {
            let mut table = Table::default();
            for (name, value) in stats.rows() {
                table.add(name, &value);
            }
            table.print(w);
        }
    }

    // statistics.go:136
    fn add_content_mapper_statistics(&self, table: &mut Table, prefix: &str) {
        let timings = &self.compile_times.content_mapper_times;
        if !timings.request_wait.is_zero() {
            table.add_duration(
                &format!("{prefix}Content mapper request wait time"),
                timings.request_wait,
            );
        }
        let mut identities: Vec<&String> = timings.mappers.keys().collect();
        identities.sort();
        for identity in identities {
            let mapper = &timings.mappers[identity];
            let initialization_count = mapper.spawn.count;
            if initialization_count != 0 {
                table.add_duration(
                    &format!("{prefix}{identity} initialization time"),
                    mapper.spawn.duration + mapper.initialize.duration,
                );
            }
            if mapper.transform.count != 0 {
                table.add_duration(
                    &format!("{prefix}{identity} transform time"),
                    mapper.transform.duration,
                );
            }
            if mapper.open_project.count != 0 {
                table.add_duration(
                    &format!("{prefix}{identity} openProject time"),
                    mapper.open_project.duration,
                );
            }
            if mapper.close_project.count != 0 {
                table.add_duration(
                    &format!("{prefix}{identity} closeProject time"),
                    mapper.close_project.duration,
                );
            }
        }
    }

    // statistics.go:155
    pub(crate) fn aggregate(&mut self, stat: &Statistics) {
        self.is_aggregate = true;
        // Aggregate statistics
        self.files += stat.files;
        self.lines += stat.lines;
        self.identifiers += stat.identifiers;
        self.symbols += stat.symbols;
        self.types += stat.types;
        self.instantiations += stat.instantiations;
        self.memory_used += stat.memory_used;
        self.memory_allocs += stat.memory_allocs;
        self.compile_times.config_time += stat.compile_times.config_time;
        self.compile_times.build_info_read_time += stat.compile_times.build_info_read_time;
        self.compile_times.parse_time += stat.compile_times.parse_time;
        self.compile_times.bind_time += stat.compile_times.bind_time;
        self.compile_times.check_time += stat.compile_times.check_time;
        self.compile_times.emit_time += stat.compile_times.emit_time;
        self.compile_times.changes_compute_time += stat.compile_times.changes_compute_time;
    }

    // statistics.go:178
    pub(crate) fn set_total_time(&mut self, total_time: Duration) {
        self.compile_times.total_time = total_time;
    }
}
