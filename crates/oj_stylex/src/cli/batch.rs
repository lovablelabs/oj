//! `batch --format ndjson` is the frozen conformance contract (conformance/README.md § Job schema);
//! `--format framed` carries the code as raw bytes behind a JSON header line instead.

use std::borrow::Cow;
use std::cell::RefCell;
use std::io::{BufRead, IoSlice, StdoutLock, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Mutex;
use std::sync::mpsc::TryRecvError;
use std::time::Instant;

use crate::api::{FileContext, parse_program, transform_program, transform_source_in};
use crate::cache::{CacheStatus, CacheStore, compile_through_cache_in};
use crate::errors::StylexError;
use crate::module_resolution::MemoFs;
use crate::options::CompilerOptions;
use crate::rules::StylexRule;
use crate::timings::{self, Stage};
use oxc_allocator::Allocator;
use oxc_codegen::Codegen;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::cli::classify::classify;
use crate::cli::queue::{Bounds, JobQueue, Weigh};
use crate::cli::wire::{OkLine, json_str, write_json, write_json_str};

#[derive(Clone, Copy)]
pub enum Backend {
    Splice,
    Ast,
}

#[derive(Clone, Copy)]
pub enum Format {
    Ndjson,
    /// Per ok result: the header line `{"id","ok":true,"codeLen":N,"rules"}`,
    /// then exactly N raw code bytes, then `\n`. Error lines stay NDJSON.
    Framed,
}

#[derive(Clone, Copy)]
pub enum Payload {
    Full,
    /// No `code` key, and no framed body either.
    Rules,
}

/// One result as written: the JSON line (newline included) and, in framed
/// mode, the unescaped code payload that follows it.
struct Emit {
    head: Vec<u8>,
    body: Option<Vec<u8>>,
}

// At one thread the writer is the compiler, so writes stay within a default
// 16 KiB pipe buffer and never wait on the reader; the writer thread batches more.
const SEQUENTIAL_FLUSH_BYTES: usize = 16 << 10;
const PARALLEL_FLUSH_BYTES: usize = 64 << 10;

/// Completed results for one `writev`; every batch ends in a newline slice, which
/// is what makes std's stdout `LineWriter` pass the whole batch to the fd unbuffered.
struct OutBatch<'a> {
    out: StdoutLock<'a>,
    items: Vec<Emit>,
    bytes: usize,
    flush_bytes: usize,
}

impl<'a> OutBatch<'a> {
    fn new(out: StdoutLock<'a>, flush_bytes: usize) -> Self {
        Self {
            out,
            items: Vec::new(),
            bytes: 0,
            flush_bytes,
        }
    }

    fn push(&mut self, item: Emit) {
        self.bytes += item.head.len() + item.body.as_ref().map_or(0, |body| body.len() + 1);
        self.items.push(item);
        if self.bytes >= self.flush_bytes {
            self.flush();
        }
    }

    fn flush(&mut self) {
        if self.items.is_empty() {
            return;
        }
        let _t = timings::start(Stage::NdjsonOut);
        let mut slices = Vec::with_capacity(self.items.len() * 3);
        for item in &self.items {
            slices.push(IoSlice::new(&item.head));
            if let Some(body) = &item.body {
                slices.push(IoSlice::new(body));
                slices.push(IoSlice::new(b"\n"));
            }
        }
        write_all_vectored(&mut self.out, &mut slices).expect("stdout write");
        self.items.clear();
        self.bytes = 0;
    }
}

// std's `Write::write_all_vectored` is still unstable.
fn write_all_vectored(out: &mut impl Write, mut bufs: &mut [IoSlice<'_>]) -> std::io::Result<()> {
    IoSlice::advance_slices(&mut bufs, 0);
    while !bufs.is_empty() {
        match out.write_vectored(bufs) {
            Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
            Ok(n) => IoSlice::advance_slices(&mut bufs, n),
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct Job {
    id: String,
    root: String,
    filename: String,
    // Exactly one of these. `sourcePath` keeps whole-repo inputs proportional to
    // the path list rather than to the sources; `filename` still names the module.
    #[serde(default)]
    source: Option<String>,
    #[serde(default, rename = "sourcePath")]
    source_path: Option<String>,
    // Raw text: option sets repeat across jobs, so resolution memoizes on it.
    #[serde(default)]
    options: Option<Box<serde_json::value::RawValue>>,
    #[serde(default)]
    #[allow(dead_code)]
    mode: Option<String>,
}

impl Job {
    fn read_source(&self) -> Result<Cow<'_, str>, String> {
        read_job_source(
            &self.root,
            self.source.as_deref(),
            self.source_path.as_deref(),
        )
    }
}

/// A queued job: the raw line, or a `sourcePath` job the reader stage already
/// parsed and read (its source moved into `source`).
enum Input {
    Line(String),
    Job(Box<Job>),
}

impl Weigh for Input {
    fn weight(&self) -> usize {
        match self {
            Input::Line(line) => line.len(),
            Input::Job(job) => {
                job.source.as_ref().map_or(0, String::len)
                    + job.source_path.as_ref().map_or(0, String::len)
                    + job.options.as_ref().map_or(0, |raw| raw.get().len())
                    + job.id.len()
                    + job.root.len()
                    + job.filename.len()
            }
        }
    }
}

/// `sourcePath` is resolved against `root`; an absolute path is used as given.
pub fn read_job_source<'a>(
    root: &str,
    source: Option<&'a str>,
    source_path: Option<&str>,
) -> Result<Cow<'a, str>, String> {
    match (source, source_path) {
        (Some(source), None) => Ok(Cow::Borrowed(source)),
        (None, Some(path)) => {
            let path = Path::new(root.trim_end_matches('/')).join(path);
            std::fs::read_to_string(&path)
                .map(Cow::Owned)
                .map_err(|err| format!("could not read {}: {err}", path.display()))
        }
        (Some(_), Some(_)) => {
            Err("job sets both source and sourcePath; exactly one is required".to_string())
        }
        (None, None) => {
            Err("job sets neither source nor sourcePath; exactly one is required".to_string())
        }
    }
}

fn usage_error(error: &str, detail: String) -> ! {
    eprintln!("{}", json!({ "error": error, "detail": detail }));
    std::process::exit(64);
}

pub struct CacheCtx {
    store: CacheStore,
    verify: bool,
}

#[derive(Default)]
struct CacheStats {
    hits: u64,
    misses: u64,
    gated: u64,
    errors: u64,
    poisoned: u64,
}

/// Per-job cache outcome; errors are never cached, so they get their own arm.
/// Poisoned carries the job id so the writer loop can report in input order.
enum CacheOutcome {
    Status(CacheStatus),
    Poisoned {
        id: String,
        mismatch: crate::cache::VerifyMismatch,
    },
    CompileError,
}

pub fn run(
    format: &str,
    backend: &str,
    emit: &str,
    threads: Option<usize>,
    timings: bool,
    cache_dir: Option<PathBuf>,
    cache_verify: bool,
) {
    let format = match format {
        "ndjson" => Format::Ndjson,
        "framed" => Format::Framed,
        other => usage_error(
            "BAD_FORMAT",
            format!("--format must be ndjson|framed, got {other}"),
        ),
    };
    let backend = match backend {
        "splice" => Backend::Splice,
        "ast" => Backend::Ast,
        other => usage_error(
            "BAD_BACKEND",
            format!("--backend must be splice|ast, got {other}"),
        ),
    };
    let payload = match emit {
        "full" => Payload::Full,
        "rules" => Payload::Rules,
        other => usage_error(
            "BAD_EMIT",
            format!("--emit must be full|rules, got {other}"),
        ),
    };
    let threads = match threads {
        Some(0) => usage_error("BAD_THREADS", "--threads must be at least 1".into()),
        Some(n) => n,
        None => std::thread::available_parallelism().map_or(1, usize::from),
    };
    let cache = resolve_cache(cache_dir, cache_verify, backend);
    let fs = MemoFs::snapshot();
    let ctx = JobCtx {
        format,
        payload,
        backend,
        cache: cache.as_ref(),
        fs: &fs,
    };
    let (walls, stats) = run_batch(&ctx, timings, threads);
    if let Some(cache) = &cache {
        eprintln!(
            "{}",
            json!({ "cache": {
                "dir": cache.store.dir().to_string_lossy(),
                "verify": cache.verify,
                "hits": stats.hits,
                "misses": stats.misses,
                "gated": stats.gated,
                "compileErrors": stats.errors,
                "poisoned": stats.poisoned,
            }})
        );
    }
    if timings {
        print_summary(&walls, threads);
    }
    timings::report_to_stderr();
    if cache.is_some_and(|cache| cache.verify) && stats.poisoned > 0 {
        eprintln!(
            "{}",
            json!({
                "error": "CACHE_VERIFY",
                "detail": format!("poisoned cache entries repaired: {}; see cacheMismatch lines", stats.poisoned),
            })
        );
        std::process::exit(1);
    }
}

// The cache stores splice-backend results, so `ast` cannot use it: an explicit
// flag conflict is a usage error; an env-only dir is skipped with a note.
fn resolve_cache(flag_dir: Option<PathBuf>, verify: bool, backend: Backend) -> Option<CacheCtx> {
    let knobs = &oj_env::get().knobs;
    let env_dir = knobs.stylex_cache_dir.clone();
    let explicit = flag_dir.is_some();
    let dir = flag_dir.or(env_dir);
    if verify && dir.is_none() {
        usage_error(
            "BAD_CACHE",
            "--cache-verify requires --cache-dir or STYLEX_CACHE_DIR".into(),
        );
    }
    let dir = dir?;
    // The kill-switch wins over everything, --cache-verify included.
    if knobs.stylex_cache_off {
        eprintln!("{}", json!({ "note": "cache disabled by STYLEX_CACHE=0" }));
        return None;
    }
    if matches!(backend, Backend::Ast) {
        if explicit {
            usage_error(
                "BAD_CACHE",
                "the compile cache supports --backend splice only".into(),
            );
        }
        eprintln!(
            "{}",
            json!({ "note": "STYLEX_CACHE_DIR ignored: the compile cache supports --backend splice only" })
        );
        return None;
    }
    match CacheStore::new(&dir) {
        Ok(store) => Some(CacheCtx { store, verify }),
        Err(err) => io_error(err),
    }
}

fn io_error(err: std::io::Error) -> ! {
    eprintln!("{}", json!({ "error": "IO", "detail": err.to_string() }));
    std::process::exit(1);
}

#[derive(Clone, Copy)]
struct JobCtx<'a> {
    format: Format,
    payload: Payload,
    backend: Backend,
    cache: Option<&'a CacheCtx>,
    fs: &'a MemoFs,
}

type Item = (Emit, u64, Option<CacheOutcome>);

// File opens contend in the kernel past a few concurrent threads, so readers
// scale slower than workers; each one keeps several compile threads fed.
fn reader_threads(threads: usize) -> usize {
    (threads / 4).clamp(1, 4)
}

/// Items a reader publishes at once, so workers see progress within a turn.
const PIECE_ITEMS: usize = 8;

/// The reader stage: stdin is taken in ordered chunks, `sourcePath` jobs are
/// parsed and read ahead of the workers, and blocks land in stdin order.
struct Feed<'a> {
    queue: &'a JobQueue<Input>,
    next_seq: Mutex<u64>,
    needle: memchr::memmem::Finder<'static>,
}

impl Feed<'_> {
    fn run(&self, readers: usize) {
        std::thread::scope(|scope| {
            for _ in 0..readers {
                scope.spawn(|| {
                    while let Some((seq, chunk)) = self.next_chunk() {
                        if !self.prefetch_publish(seq, chunk) {
                            break;
                        }
                    }
                });
            }
        });
        self.queue.finish();
    }

    // The sequence counter stays locked across the stdin reads, so chunk order
    // is sequence order.
    fn next_chunk(&self) -> Option<(u64, Vec<String>)> {
        let bounds = self.queue.bounds();
        let mut next_seq = self.next_seq.lock().expect("feed lock");
        let stdin = std::io::stdin();
        let mut stdin = stdin.lock();
        let mut chunk = Vec::new();
        let mut bytes = 0;
        while chunk.len() < bounds.block_lines && bytes < bounds.block_bytes {
            let mut line = String::new();
            match stdin.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {}
                Err(err) => io_error(err),
            }
            if line.ends_with('\n') {
                line.pop();
                if line.ends_with('\r') {
                    line.pop();
                }
            }
            if line.trim().is_empty() {
                continue;
            }
            bytes += line.len();
            chunk.push(line);
        }
        if chunk.is_empty() {
            return None;
        }
        let seq = *next_seq;
        *next_seq += 1;
        Some((seq, chunk))
    }

    fn prefetch_publish(&self, seq: u64, chunk: Vec<String>) -> bool {
        let block_bytes = self.queue.bounds().block_bytes;
        let count = chunk.len();
        let mut block = Vec::with_capacity(PIECE_ITEMS);
        let mut weight = 0;
        for (k, line) in chunk.into_iter().enumerate() {
            let input = self.prefetch(line);
            weight += input.weight();
            block.push(input);
            let last = k + 1 == count;
            if last || block.len() >= PIECE_ITEMS || weight >= block_bytes {
                if !self.queue.publish_seq(seq, last, &mut block) {
                    return false;
                }
                weight = 0;
            }
        }
        true
    }

    // Starving workers get the raw line and read for themselves; otherwise only
    // `sourcePath` lines are parsed here (an inline `source` parse stays on the workers).
    fn prefetch(&self, line: String) -> Input {
        if self.queue.backlog() < self.queue.bounds().low_water
            || self.needle.find(line.as_bytes()).is_none()
        {
            return Input::Line(line);
        }
        let job = {
            let _t = timings::start(Stage::NdjsonIn);
            serde_json::from_str::<Job>(&line)
        };
        let Ok(mut job) = job else {
            return Input::Line(line);
        };
        // A failed read stays with the worker, which re-reads and reports it.
        if job.source.is_none()
            && let Some(path) = job.source_path.as_deref()
            && let Ok(source) = read_job_source(&job.root, None, Some(path))
        {
            job.source = Some(source.into_owned());
            job.source_path = None;
        }
        Input::Job(Box::new(job))
    }
}

/// Stops the readers when the consuming thread unwinds, so `thread::scope`
/// never waits on a reader blocked at the backlog bound.
struct AbortOnDrop<'a>(&'a JobQueue<Input>);

impl Drop for AbortOnDrop<'_> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

// Result i is written once all j < i are: bytes match at every thread count,
// and the reorder buffer holds only results that outran an in-flight job.
fn run_batch(ctx: &JobCtx<'_>, timings: bool, threads: usize) -> (Vec<u64>, CacheStats) {
    let queue = JobQueue::new(Bounds::for_threads(threads));
    let feed = Feed {
        queue: &queue,
        next_seq: Mutex::new(0),
        needle: memchr::memmem::Finder::new(b"\"sourcePath\""),
    };
    let stdout = std::io::stdout();
    let flush_bytes = if threads == 1 {
        SEQUENTIAL_FLUSH_BYTES
    } else {
        PARALLEL_FLUSH_BYTES
    };
    let mut out = OutBatch::new(stdout.lock(), flush_bytes);
    let mut walls = Vec::new();
    let mut stats = CacheStats::default();
    let mut emit = |(mut result, wall_ns, status): Item, out: &mut OutBatch<'_>| {
        record_status(&mut stats, status);
        if timings {
            attach_wall(&mut result.head, wall_ns);
            walls.push(wall_ns);
        }
        out.push(result);
    };
    std::thread::scope(|scope| {
        let (queue, feed) = (&queue, &feed);
        scope.spawn(move || feed.run(reader_threads(threads)));
        if threads == 1 {
            let _abort = AbortOnDrop(queue);
            while let Some((_, input)) = queue.claim() {
                emit(run_input(input, ctx), &mut out);
            }
            return;
        }
        let (tx, rx) = std::sync::mpsc::sync_channel::<(usize, Item)>(threads * 4);
        for _ in 0..threads {
            let tx = tx.clone();
            scope.spawn(move || {
                while let Some((i, input)) = queue.claim() {
                    // A send error means the writer is gone (panic): stop.
                    if tx.send((i, run_input(input, ctx))).is_err() {
                        queue.abort();
                        break;
                    }
                }
            });
        }
        drop(tx);
        let mut next_write = 0usize;
        let mut pending: std::collections::BTreeMap<usize, Item> =
            std::collections::BTreeMap::new();
        loop {
            // Held results go out before the writer blocks: the syscall lands
            // while the workers have nothing ready, and the readers stay fed.
            let (i, item) = match rx.try_recv() {
                Ok(msg) => msg,
                Err(TryRecvError::Empty) => {
                    out.flush();
                    match rx.recv() {
                        Ok(msg) => msg,
                        Err(_) => break,
                    }
                }
                Err(TryRecvError::Disconnected) => break,
            };
            if i != next_write {
                pending.insert(i, item);
                continue;
            }
            emit(item, &mut out);
            next_write += 1;
            while let Some(item) = pending.remove(&next_write) {
                emit(item, &mut out);
                next_write += 1;
            }
        }
    });
    out.flush();
    (walls, stats)
}

// Poisoned-entry reports land on stderr in input order, never on stdout: the
// result stream stays byte-identical across cache states.
fn record_status(stats: &mut CacheStats, outcome: Option<CacheOutcome>) {
    match outcome {
        None => {}
        Some(CacheOutcome::CompileError) => stats.errors += 1,
        Some(CacheOutcome::Status(CacheStatus::Hit)) => stats.hits += 1,
        Some(CacheOutcome::Status(CacheStatus::Miss)) => stats.misses += 1,
        Some(CacheOutcome::Status(CacheStatus::Gated)) => stats.gated += 1,
        Some(CacheOutcome::Status(CacheStatus::Poisoned(_))) => {
            unreachable!("run_job rewraps Poisoned with its job id")
        }
        Some(CacheOutcome::Poisoned { id, mismatch }) => {
            stats.poisoned += 1;
            eprintln!(
                "{}",
                json!({ "cacheMismatch": {
                    "id": id,
                    "field": mismatch.field,
                    "cached": mismatch.cached,
                    "fresh": mismatch.fresh,
                }})
            );
        }
    }
}

fn run_input(input: Input, ctx: &JobCtx<'_>) -> Item {
    let start = Instant::now();
    let _t = timings::start(Stage::JobTotal);
    let job = match input {
        Input::Line(line) => {
            let _t = timings::start(Stage::NdjsonIn);
            serde_json::from_str::<Job>(&line)
        }
        Input::Job(job) => Ok(*job),
    };
    let (result, outcome) = match job {
        Ok(mut job) => run_job(&mut job, ctx),
        Err(err) => (
            line_only(
                json!({
                    "id": "?",
                    "ok": false,
                    "errorClass": "unknown",
                    "message": format!("malformed job line: {err}"),
                })
                .to_string(),
            ),
            ctx.cache.map(|_| CacheOutcome::CompileError),
        ),
    };
    (result, start.elapsed().as_nanos() as u64, outcome)
}

fn line_only(json: String) -> Emit {
    let mut head = json.into_bytes();
    head.push(b'\n');
    Emit { head, body: None }
}

// Byte parity with the previous Value-based line: a last-inserted map key
// serialized last, and u64 Display matches serde_json's integer output.
fn attach_wall(head: &mut Vec<u8>, wall_ns: u64) {
    debug_assert!(head.ends_with(b"}\n"));
    head.truncate(head.len() - 2);
    writeln!(head, ",\"wall_ns\":{wall_ns}}}").expect("in-memory write");
}

fn print_summary(walls: &[u64], threads: usize) {
    let mut sorted = walls.to_vec();
    sorted.sort_unstable();
    let percentile = |q: f64| -> u64 {
        if sorted.is_empty() {
            return 0;
        }
        let rank = ((sorted.len() as f64) * q).ceil() as usize;
        sorted[rank.clamp(1, sorted.len()) - 1]
    };
    let total: u64 = sorted.iter().sum();
    eprintln!(
        "{}",
        json!({ "timings": {
            "jobs": sorted.len(),
            "threads": threads,
            "p50_ns": percentile(0.50),
            "p95_ns": percentile(0.95),
            "total_ns": total,
        }})
    );
}

fn run_job(job: &mut Job, ctx: &JobCtx<'_>) -> (Emit, Option<CacheOutcome>) {
    let source = match job.read_source() {
        Ok(source) => source,
        Err(detail) => {
            return (
                job_input_error(&job.id, &detail),
                ctx.cache.map(|_| CacheOutcome::CompileError),
            );
        }
    };
    match compile(job, &source, ctx) {
        Ok((code, rules, outcome)) => {
            let outcome = match outcome {
                Some(CacheOutcome::Status(CacheStatus::Poisoned(mismatch))) => {
                    Some(CacheOutcome::Poisoned {
                        id: job.id.clone(),
                        mismatch,
                    })
                }
                other => other,
            };
            let _t = timings::start(Stage::NdjsonOut);
            let result = match (ctx.payload, ctx.format) {
                (Payload::Rules, _) => {
                    line_only(OkLine::new(&json_str(&job.id), 0).rules(&rules).finish())
                }
                (Payload::Full, Format::Ndjson) => Emit {
                    head: result_line_ok(&job.id, &code, &rules),
                    body: None,
                },
                (Payload::Full, Format::Framed) => {
                    let head = framed_head_ok(&job.id, code.len(), &rules);
                    // A pass-through result is the source itself: hand over
                    // whichever buffer already owns it instead of copying.
                    let body = match code {
                        Cow::Owned(code) => code,
                        Cow::Borrowed(_) => match source {
                            Cow::Owned(source) => source,
                            Cow::Borrowed(_) => job.source.take().expect("borrowed from job"),
                        },
                    };
                    Emit {
                        head,
                        body: Some(body.into_bytes()),
                    }
                }
            };
            (result, outcome)
        }
        Err(error) => job_error(job, error, ctx.cache),
    }
}

// A bad job line is a protocol failure, not a source-level one: it never earns
// a StyleX error class, and it must fail the differ rather than look compilable.
fn job_input_error(id: &str, detail: &str) -> Emit {
    line_only(
        json!({ "id": id, "ok": false, "errorClass": "unknown", "message": detail }).to_string(),
    )
}

fn job_error(
    job: &Job,
    error: StylexError,
    cache: Option<&CacheCtx>,
) -> (Emit, Option<CacheOutcome>) {
    // Babel prefixes every plugin error with the filename; the class regexes
    // (one is anchored) run over that prefixed message.
    let first_line = error.message.lines().next().unwrap_or_default();
    let root = job.root.trim_end_matches('/');
    let prefixed = format!("{root}/{}: {first_line}", job.filename);
    let wire = StylexError::new(error.code, prefixed);
    (
        line_only(
            json!({
                "id": job.id,
                "ok": false,
                "errorClass": classify(&wire),
                "message": wire.message.replace(root, "<ROOT>"),
            })
            .to_string(),
        ),
        cache.map(|_| CacheOutcome::CompileError),
    )
}

// The exact bytes `json!` + `to_metadata_tuple` produced, minus their `Value`
// tree: serde_json escaping/float formatting is shared with the Value path.
fn result_line_ok(id: &str, code: &str, rules: &[StylexRule]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(code.len() + code.len() / 4 + 128);
    buf.extend_from_slice(b"{\"id\":");
    write_json_str(&mut buf, id);
    buf.extend_from_slice(b",\"ok\":true,\"code\":");
    write_json_str(&mut buf, code);
    buf.extend_from_slice(b",\"rules\":[");
    write_rules(&mut buf, rules);
    buf.extend_from_slice(b"]}\n");
    buf
}

fn framed_head_ok(id: &str, code_len: usize, rules: &[StylexRule]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(rules.len() * 96 + 128);
    buf.extend_from_slice(b"{\"id\":");
    write_json_str(&mut buf, id);
    write!(buf, ",\"ok\":true,\"codeLen\":{code_len},\"rules\":[").expect("in-memory write");
    write_rules(&mut buf, rules);
    buf.extend_from_slice(b"]}\n");
    buf
}

fn write_rules(buf: &mut Vec<u8>, rules: &[StylexRule]) {
    for (i, rule) in rules.iter().enumerate() {
        if i > 0 {
            buf.push(b',');
        }
        buf.push(b'[');
        write_json_str(buf, &rule.class_name);
        buf.extend_from_slice(b",{\"ltr\":");
        write_json_str(buf, &rule.ltr);
        if let Some(rtl) = &rule.rtl {
            buf.extend_from_slice(b",\"rtl\":");
            write_json_str(buf, rtl);
        }
        if let Some(const_key) = &rule.const_key {
            buf.extend_from_slice(b",\"constKey\":");
            write_json_str(buf, const_key);
        }
        if let Some(const_val) = &rule.const_val {
            buf.extend_from_slice(b",\"constVal\":");
            write_json(buf, const_val);
        }
        buf.extend_from_slice(b"},");
        write_json(buf, &rule.priority);
        buf.push(b']');
    }
}

thread_local! {
    // One arena per worker thread, reset between jobs, shared by both backends
    // (transform_source_in / compile_through_cache_in on splice).
    pub static ARENA: RefCell<Allocator> = RefCell::new(Allocator::default());
    // Option sets repeat across a batch (dev/prod pairs): raw text → resolved.
    static OPTIONS_MEMO: RefCell<crate::fxhash::FxHashMap<String, Rc<crate::options::ResolvedOptions>>> =
        RefCell::new(crate::fxhash::FxHashMap::default());
}

pub fn resolve_options_memoized(
    raw: Option<&serde_json::value::RawValue>,
) -> Result<Rc<crate::options::ResolvedOptions>, StylexError> {
    let text = raw.map_or("", |r| r.get());
    OPTIONS_MEMO.with(|memo| {
        let mut memo = memo.borrow_mut();
        if let Some(hit) = memo.get(text) {
            return Ok(Rc::clone(hit));
        }
        let value: Value = match raw {
            Some(raw) => serde_json::from_str(raw.get())
                .map_err(|err| StylexError::parse_error(&err.to_string()))?,
            None => Value::Null,
        };
        let resolved = Rc::new(CompilerOptions::from_json(&value)?.resolve()?);
        memo.insert(text.to_string(), Rc::clone(&resolved));
        Ok(resolved)
    })
}

type Compiled<'a> = (Cow<'a, str>, Vec<StylexRule>, Option<CacheOutcome>);

fn compile<'a>(job: &Job, source: &'a str, ctx: &JobCtx<'_>) -> Result<Compiled<'a>, StylexError> {
    let options = {
        let _t = timings::start(Stage::Options);
        resolve_options_memoized(job.options.as_deref())?
    };
    let root = job.root.trim_end_matches('/');
    let filename = format!("{root}/{}", job.filename);
    let file = FileContext {
        filename: Path::new(&filename),
        source_text: source,
        cwd: Path::new(root),
    };
    let fs = ctx.fs;
    match ctx.backend {
        Backend::Splice => ARENA.with(|cell| {
            let mut allocator = cell.borrow_mut();
            allocator.reset();
            if let Some(cache) = ctx.cache {
                let (result, status) = compile_through_cache_in(
                    &allocator,
                    &cache.store,
                    &file,
                    &options,
                    fs,
                    cache.verify,
                )?;
                let outcome = Some(CacheOutcome::Status(status));
                return match result {
                    Some(result) => Ok((Cow::Owned(result.code), result.rules, outcome)),
                    None => Ok((Cow::Borrowed(source), Vec::new(), outcome)),
                };
            }
            match transform_source_in(&allocator, &file, &options, fs)? {
                Some(result) => Ok((Cow::Owned(result.code), result.rules, None)),
                None => Ok((Cow::Borrowed(source), Vec::new(), None)),
            }
        }),
        Backend::Ast => ARENA.with(|cell| {
            let mut allocator = cell.borrow_mut();
            allocator.reset();
            let mut program = {
                let _t = timings::start(Stage::Parse);
                parse_program(&allocator, source, &filename)?
            };
            let result = transform_program(&allocator, &mut program, &file, &options, fs)?;
            // Untouched programs pass through byte-identical, like the splice
            // backend; only modified ones go through codegen.
            let code = if result.modified && matches!(ctx.payload, Payload::Full) {
                let _t = timings::start(Stage::PrintAst);
                Cow::Owned(Codegen::new().build(&program).code)
            } else {
                Cow::Borrowed(source)
            };
            Ok((code, result.rules, None))
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes at most three bytes per call and fails with `Interrupted` once.
    struct Choppy {
        got: Vec<u8>,
        interrupted: bool,
    }

    impl Write for Choppy {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if !self.interrupted {
                self.interrupted = true;
                return Err(std::io::ErrorKind::Interrupted.into());
            }
            let n = buf.len().min(3);
            self.got.extend_from_slice(&buf[..n]);
            Ok(n)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn write_all_vectored_survives_short_writes_and_empty_slices() {
        let parts: [&[u8]; 5] = [b"", b"{\"id\":1}\n", b"", b"body bytes", b"\n"];
        let mut slices: Vec<IoSlice<'_>> = parts.iter().map(|p| IoSlice::new(p)).collect();
        let mut out = Choppy {
            got: Vec::new(),
            interrupted: false,
        };
        write_all_vectored(&mut out, &mut slices).expect("write");
        assert_eq!(out.got, b"{\"id\":1}\nbody bytes\n");
        let mut empty: Vec<IoSlice<'_>> = vec![IoSlice::new(b"")];
        write_all_vectored(&mut out, &mut empty).expect("nothing to write");
    }

    #[test]
    fn attach_wall_rewrites_the_line_end() {
        let mut head = b"{\"id\":\"a\",\"ok\":true,\"codeLen\":0,\"rules\":[]}\n".to_vec();
        attach_wall(&mut head, 42);
        assert_eq!(
            head,
            b"{\"id\":\"a\",\"ok\":true,\"codeLen\":0,\"rules\":[],\"wall_ns\":42}\n"
        );
    }

    // `fed` items already sit in the queue when the chunk is prefetched: at or
    // above the low-water mark the readers prefetch, below it they pass through.
    fn feed_over(bounds: Bounds, fed: usize, lines: &[&str]) -> Vec<Input> {
        let queue = JobQueue::new(bounds);
        let feed = Feed {
            queue: &queue,
            next_seq: Mutex::new(0),
            needle: memchr::memmem::Finder::new(b"\"sourcePath\""),
        };
        let mut filler: Vec<Input> = (0..fed).map(|_| Input::Line(String::new())).collect();
        assert!(queue.publish_seq(0, true, &mut filler));
        let chunk: Vec<String> = lines.iter().map(|line| line.to_string()).collect();
        assert!(feed.prefetch_publish(1, chunk));
        queue.finish();
        let mut got = Vec::new();
        while let Some((i, input)) = queue.claim() {
            if i >= fed {
                assert_eq!(i - fed, got.len());
                got.push(input);
            }
        }
        got
    }

    #[test]
    fn prefetch_passes_lines_through_while_the_workers_starve() {
        let bounds = Bounds::for_threads(1);
        let job = r#"{"id":"p","root":"/r","filename":"a.ts","sourcePath":"a.ts"}"#;
        let got = feed_over(bounds, bounds.low_water - 1, &[job]);
        assert!(matches!(&got[0], Input::Line(line) if line == job));
    }

    #[test]
    fn prefetch_reads_source_path_jobs_and_leaves_the_rest_alone() {
        let dir = std::env::temp_dir().join("fru-batch-prefetch");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("a.ts"), "export const a = 1;\n").expect("write");
        let root = dir.to_string_lossy().replace('\\', "/");
        let by_path =
            format!(r#"{{"id":"p","root":"{root}","filename":"a.ts","sourcePath":"a.ts"}}"#);
        let missing =
            format!(r#"{{"id":"m","root":"{root}","filename":"b.ts","sourcePath":"b.ts"}}"#);
        let inline = r#"{"id":"i","root":"/r","filename":"x.ts","source":"sourcePath is text"}"#;
        let broken = r#"{"id":"b","root":"/r","sourcePath":"a.ts""#;
        let bounds = Bounds::for_threads(1);
        let got = feed_over(
            bounds,
            bounds.low_water,
            &[&by_path, &missing, inline, broken],
        );
        match &got[0] {
            Input::Job(job) => {
                assert_eq!(job.source.as_deref(), Some("export const a = 1;\n"));
                assert!(job.source_path.is_none());
            }
            Input::Line(_) => panic!("sourcePath job not prefetched"),
        }
        match &got[1] {
            Input::Job(job) => {
                assert!(job.source.is_none());
                assert_eq!(job.source_path.as_deref(), Some("b.ts"));
            }
            Input::Line(_) => panic!("missing file should still be a parsed job"),
        }
        assert!(matches!(&got[2], Input::Line(line) if line == inline));
        assert!(matches!(&got[3], Input::Line(line) if line == broken));
    }

    #[test]
    fn prefetch_publishes_a_chunk_in_pieces_at_the_block_bound() {
        let bounds = Bounds::for_threads(1);
        let big = "x".repeat(bounds.block_bytes);
        let lines: Vec<String> = (0..5)
            .map(|k| format!(r#"{{"id":"{k}","root":"/r","filename":"x.ts","source":"{big}"}}"#))
            .collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let got = feed_over(bounds, bounds.low_water, &refs);
        assert_eq!(got.len(), 5);
        for (k, input) in got.iter().enumerate() {
            assert!(matches!(input, Input::Line(line) if line == &lines[k]));
        }
    }
}
