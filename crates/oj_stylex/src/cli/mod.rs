//! The StyleX command line, shared by the `fru` binary and `oj stylex`.

mod assemble;
mod batch;
mod classify;
mod compile;
mod extract;
mod queue;
mod reprint;
mod serve;
mod transform;
mod wire;

use std::path::PathBuf;

use clap::Subcommand;

/// The StyleX command set, shared by the `fru` binary and `oj stylex`.
#[derive(Subcommand)]
pub enum Command {
    /// Transform one file: source on stdin, compiled code on stdout.
    Transform {
        /// Absolute filename the source pretends to live at (default: cwd/input.tsx).
        #[arg(long)]
        filename: Option<String>,
        /// process.cwd() equivalent (default: the actual cwd).
        #[arg(long)]
        cwd: Option<String>,
        /// Raw @stylexjs/babel-plugin options as JSON (default: {}).
        #[arg(long)]
        options: Option<String>,
        /// Write the metadata.stylex rule tuples as JSON to this file.
        #[arg(long)]
        rules_out: Option<PathBuf>,
        /// `code` (default): compiled code on stdout. `json`: one batch-shaped
        /// result line `{id, ok, code, rules, map}` on stdout.
        #[arg(long)]
        emit: Option<String>,
    },
    /// Persistent NDJSON request/response loop on stdin/stdout with a resident
    /// rule registry (the dev-server embedding; see docs/serve-protocol.md).
    Serve {
        /// Speak the protocol over stdin/stdout (the only transport).
        #[arg(long)]
        stdio: bool,
        /// Worker threads for concurrent requests and the `warm` walk
        /// (default: all logical cores).
        #[arg(long)]
        threads: Option<usize>,
        /// Add per-request `wall_ns` to each response line.
        #[arg(long)]
        timings: bool,
    },
    /// Walk a project, compile every StyleX file, emit one CSS sheet
    Compile {
        /// Project root to walk (default: current directory)
        #[arg(default_value = ".")]
        root: std::path::PathBuf,
        /// Raw plugin options as JSON (default: commonJS resolution at the root)
        #[arg(long)]
        options: Option<String>,
        /// Write the sheet here instead of stdout
        #[arg(long)]
        out: Option<std::path::PathBuf>,
        #[arg(long, default_value_t = num_threads())]
        threads: usize,
        /// Compile cache directory (fallback: STYLEX_CACHE_DIR; neither = cache
        /// off). STYLEX_CACHE=0 force-disables.
        #[arg(long)]
        cache_dir: Option<std::path::PathBuf>,
    },
    /// NDJSON job batch on stdin, NDJSON results on stdout (conformance contract)
    Batch {
        /// `ndjson` (the conformance contract) or `framed`: per ok result a
        /// header line with `codeLen`, then that many raw code bytes, then `\n`.
        #[arg(long)]
        format: String,
        /// Output backend: `splice` (span edits over the source text) or
        /// `ast` (transform_program + oxc_codegen, the oj dev path).
        #[arg(long, default_value = "splice")]
        backend: String,
        /// `full` (default): `code` and `rules` per ok result. `rules`: no
        /// `code` key (and no framed body), for rule-only consumers.
        #[arg(long, default_value = "full")]
        emit: String,
        /// Compile threads (default: all logical cores). A small reader pool
        /// feeds them; results are always emitted in input order, byte-identical.
        #[arg(long)]
        threads: Option<usize>,
        /// Add per-job `wall_ns` to each result line and print a p50/p95/total
        /// summary to stderr.
        #[arg(long)]
        timings: bool,
        /// Compile cache directory (fallback: STYLEX_CACHE_DIR; neither = cache
        /// off). Splice backend only; STYLEX_CACHE=0 force-disables the cache.
        #[arg(long)]
        cache_dir: Option<PathBuf>,
        /// Recompile every cache hit and byte-compare: mismatches are reported
        /// on stderr, the entries repaired, and the run exits 1 on any.
        #[arg(long)]
        cache_verify: bool,
    },
    /// Assemble CSS from NDJSON rule metadata tuples on stdin.
    Assemble {
        /// Raw processStylexRules config as JSON (default: {}).
        #[arg(long)]
        config: Option<String>,
        /// Write {rules, parse_ns, assemble_ns, bytes} JSON to stderr.
        #[arg(long)]
        timings: bool,
        /// HMR-path bench: N single-file swaps through RuleRegistry; prints
        /// per-delta micros JSON to stdout instead of CSS.
        #[arg(long, value_name = "N")]
        bench_incremental: Option<u32>,
        /// File count the rules are partitioned into for --bench-incremental
        /// (default mirrors the shadow corpus).
        #[arg(long, default_value_t = 3329)]
        bench_files: usize,
    },
    /// Parse + reprint stdin through oxc_codegen without transforming.
    Reprint {
        /// Filename that picks the dialect by extension (default: TSX).
        #[arg(long)]
        filename: Option<String>,
    },
    /// Compiled create() outputs as canonical NDJSON (shadow-gate surface).
    ExtractObjects {
        #[arg(long)]
        filename: Option<String>,
        #[arg(long)]
        cwd: Option<String>,
        #[arg(long)]
        options: Option<String>,
    },
    /// Probe whether stdin parses as TS/TSX; exit 0/1, no stdout.
    ParseCheck {
        /// Filename that picks the dialect by extension (default: TSX).
        #[arg(long)]
        filename: Option<String>,
    },
    /// murmur2(seed 1) -> base36 of the given string.
    Hash { input: String },
    /// createShortHash (base62 of murmur2 % 62^5) of the given string.
    Minkey { input: String },
}

fn num_threads() -> usize {
    std::thread::available_parallelism().map_or(1, usize::from)
}

pub fn run(command: Command) {
    match command {
        Command::Hash { input } => println!("{}", crate::hash::hash(&input)),
        Command::Minkey { input } => {
            println!("{}", crate::hash::create_short_hash(&input))
        }
        Command::Reprint { filename } => reprint::run(reprint::Mode::Reprint, filename.as_deref()),
        Command::ParseCheck { filename } => {
            reprint::run(reprint::Mode::ParseCheck, filename.as_deref())
        }
        Command::Transform {
            filename,
            cwd,
            options,
            rules_out,
            emit,
        } => transform::run(
            filename.as_deref(),
            cwd.as_deref(),
            options.as_deref(),
            rules_out.as_deref(),
            emit.as_deref(),
        ),
        Command::Serve {
            stdio,
            threads,
            timings,
        } => serve::run(stdio, threads, timings),
        Command::Compile {
            root,
            options,
            out,
            threads,
            cache_dir,
        } => compile::run(
            &root,
            options.as_deref(),
            out.as_deref(),
            threads,
            cache_dir.as_deref(),
        ),
        Command::Batch {
            format,
            backend,
            emit,
            threads,
            timings,
            cache_dir,
            cache_verify,
        } => batch::run(
            &format,
            &backend,
            &emit,
            threads,
            timings,
            cache_dir,
            cache_verify,
        ),
        Command::ExtractObjects {
            filename,
            cwd,
            options,
        } => extract::run(filename.as_deref(), cwd.as_deref(), options.as_deref()),
        Command::Assemble {
            config,
            timings,
            bench_incremental,
            bench_files,
        } => assemble::run(config.as_deref(), timings, bench_incremental, bench_files),
    }
}
