//! `serve --stdio`: a resident NDJSON request loop with an in-process [`RuleRegistry`]
//! for the Vite dev plugin; responses correlate by `id`, never by order (docs/serve-protocol.md).

use std::borrow::Cow;
use std::io::{BufRead, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError, mpsc};
use std::time::Instant;

use crate::api::{
    FileContext, MapMode, might_contain_stylex, transform_source_in, transform_source_in_with_map,
};
use crate::assemble::{AssembleConfig, RuleRegistry};
use crate::module_resolution::MemoFs;
use crate::options::CompilerOptions;
use crate::rules::StylexRule;
use crate::timings::{self, Stage};
use oxc_allocator::Allocator;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::cli::batch::{ARENA, read_job_source, resolve_options_memoized};
use crate::cli::compile;
use crate::cli::queue::{Bounds, JobQueue};
use crate::cli::wire::{OkLine, compile_error_line, input_error_line, protocol_error_line};

// A worker keeps its arena between requests; one that grew for a large file is
// dropped so a long dev session does not pin every thread's high-water mark.
const ARENA_CAP_BYTES: usize = 4 << 20;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Request {
    #[serde(default)]
    id: Option<Value>,
    #[serde(default)]
    op: Option<String>,
    #[serde(default)]
    root: Option<String>,
    #[serde(default)]
    filename: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    source_path: Option<String>,
    #[serde(default)]
    options: Option<Box<serde_json::value::RawValue>>,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    want_code: Option<bool>,
    #[serde(default)]
    want_map: Option<bool>,
    #[serde(default)]
    rules: Option<Vec<Value>>,
    #[serde(default)]
    config: Option<Value>,
    #[serde(default)]
    roots: Option<Vec<PathBuf>>,
}

struct Shared {
    registry: Mutex<RuleRegistry>,
    threads: usize,
}

struct Response {
    line: String,
    shutdown: bool,
}

fn usage_error(error: &str, detail: &str) -> ! {
    eprintln!("{}", json!({ "error": error, "detail": detail }));
    std::process::exit(64);
}

pub fn run(stdio: bool, threads: Option<usize>, timings_flag: bool) {
    if !stdio {
        usage_error("BAD_TRANSPORT", "serve speaks NDJSON over --stdio only");
    }
    let threads = match threads {
        Some(0) => usage_error("BAD_THREADS", "--threads must be at least 1"),
        Some(n) => n,
        None => std::thread::available_parallelism().map_or(1, usize::from),
    };
    let shared = Shared {
        registry: Mutex::new(RuleRegistry::new()),
        threads,
    };
    let queue: JobQueue<String> = JobQueue::new(Bounds::for_threads(threads));
    let (tx, rx) = mpsc::channel::<(usize, Response)>();
    std::thread::scope(|scope| {
        let (shared, queue) = (&shared, &queue);
        for _ in 0..threads {
            let tx = tx.clone();
            scope.spawn(move || {
                while let Some((i, line)) = queue.claim() {
                    let started = Instant::now();
                    let mut response = handle(&line, shared);
                    if timings_flag {
                        attach_wall(&mut response.line, started.elapsed().as_nanos() as u64);
                    }
                    if tx.send((i, response)).is_err() {
                        queue.abort();
                        break;
                    }
                }
            });
        }
        drop(tx);
        scope.spawn(move || write_responses(&rx));
        let stdin = std::io::stdin();
        let mut block = Vec::with_capacity(1);
        for line in stdin.lock().lines() {
            let line = match line {
                Ok(line) => line,
                Err(err) => {
                    eprintln!("{}", json!({ "error": "IO", "detail": err.to_string() }));
                    std::process::exit(1);
                }
            };
            if line.trim().is_empty() {
                continue;
            }
            block.push(line);
            if !queue.publish(&mut block) {
                break;
            }
        }
        queue.finish();
    });
    timings::report_to_stderr();
}

// Finished responses are written and flushed together (an idle daemon flushes at
// once); a shutdown is answered only after every request read before it, then exits.
fn write_responses(rx: &mpsc::Receiver<(usize, Response)>) {
    let stdout = std::io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    let mut done_below = 0usize;
    let mut done_ahead = std::collections::BTreeSet::new();
    let mut shutdown: Option<(usize, String)> = None;
    for first in rx.iter() {
        for (i, response) in std::iter::once(first).chain(rx.try_iter()) {
            if i == done_below {
                done_below += 1;
                while done_ahead.remove(&done_below) {
                    done_below += 1;
                }
            } else {
                done_ahead.insert(i);
            }
            if response.shutdown {
                shutdown = Some((i, response.line));
                continue;
            }
            write_line(&mut out, &response.line);
        }
        if let Some((at, line)) = &shutdown
            && done_below > *at
        {
            write_line(&mut out, line);
            if out.flush().is_err() {
                std::process::exit(1);
            }
            timings::report_to_stderr();
            std::process::exit(0);
        }
        if out.flush().is_err() {
            std::process::exit(1);
        }
    }
}

fn write_line(out: &mut BufWriter<std::io::StdoutLock<'_>>, line: &str) {
    if out
        .write_all(line.as_bytes())
        .and_then(|()| out.write_all(b"\n"))
        .is_err()
    {
        std::process::exit(1);
    }
}

fn attach_wall(line: &mut String, wall_ns: u64) {
    debug_assert!(line.ends_with('}'));
    line.pop();
    line.push_str(&format!(",\"wall_ns\":{wall_ns}}}"));
}

fn handle(line: &str, shared: &Shared) -> Response {
    let _t = timings::start(Stage::JobTotal);
    let request = {
        let _t = timings::start(Stage::NdjsonIn);
        serde_json::from_str::<Request>(line)
    };
    let request = match request {
        Ok(request) => request,
        Err(err) => {
            return Response {
                line: input_error_line("null", &format!("malformed request line: {err}")),
                shutdown: false,
            };
        }
    };
    let id = request
        .id
        .as_ref()
        .map_or_else(|| "null".to_string(), Value::to_string);
    let line = match request.op.as_deref().unwrap_or("transform") {
        "transform" => transform(&request, &id, shared),
        "setFileRules" => set_file_rules(&request, &id, shared),
        "removeFile" => remove_file(&request, &id, shared),
        "sheet" => sheet(&request, &id, shared),
        "warm" => warm(&request, &id, shared),
        "hello" => OkLine::new(&id, 0)
            .str("fingerprint", crate::cache::compiler_fingerprint())
            .value("threads", &shared.threads)
            .value("pid", &std::process::id())
            .finish(),
        "shutdown" => {
            return Response {
                line: OkLine::new(&id, 0).finish(),
                shutdown: true,
            };
        }
        other => protocol_error_line(&id, "BAD_OP", &format!("unknown op {other}")),
    };
    Response {
        line,
        shutdown: false,
    }
}

fn lock(registry: &Mutex<RuleRegistry>) -> std::sync::MutexGuard<'_, RuleRegistry> {
    registry.lock().unwrap_or_else(PoisonError::into_inner)
}

fn register(registry: &Mutex<RuleRegistry>, key: &str, rules: Vec<StylexRule>) -> (u64, bool) {
    let mut registry = lock(registry);
    let before = registry.generation();
    registry.set_file_rules(key, rules);
    let after = registry.generation();
    (after, after != before)
}

// Same inputs and result bytes as a `batch` job; `key` additionally publishes
// the rules to the registry, `wantMap` appends a mappings-only sourcemap.
fn transform(request: &Request, id: &str, shared: &Shared) -> String {
    let (Some(root), Some(filename)) = (&request.root, &request.filename) else {
        return input_error_line(id, "transform needs root and filename");
    };
    let source = match read_job_source(
        root,
        request.source.as_deref(),
        request.source_path.as_deref(),
    ) {
        Ok(source) => source,
        Err(detail) => return input_error_line(id, &detail),
    };
    let options = {
        let _t = timings::start(Stage::Options);
        match resolve_options_memoized(request.options.as_deref()) {
            Ok(options) => options,
            Err(error) => return compile_error_line(id, root, filename, error),
        }
    };
    let root_dir = root.trim_end_matches('/');
    let full_name = format!("{root_dir}/{filename}");
    let ctx = FileContext {
        filename: Path::new(&full_name),
        source_text: &source,
        cwd: Path::new(root_dir),
    };
    let map_mode = match request.want_map {
        Some(true) => MapMode::Mappings,
        _ => MapMode::Off,
    };
    let compiled = ARENA.with(|cell| {
        let mut allocator = cell.borrow_mut();
        allocator.reset();
        let result =
            transform_source_in_with_map(&allocator, &ctx, &options, MemoFs::shared(), map_mode);
        if allocator.capacity() > ARENA_CAP_BYTES {
            *allocator = Allocator::default();
        }
        result
    });
    let (code, rules, map) = match compiled {
        Ok(Some(result)) => (Cow::Owned(result.code), result.rules, result.map),
        Ok(None) => (Cow::Borrowed(&*source), Vec::new(), None),
        Err(error) => return compile_error_line(id, root, filename, error),
    };
    let _t = timings::start(Stage::NdjsonOut);
    let want_code = request.want_code != Some(false);
    let mut line = OkLine::new(
        id,
        if want_code {
            code.len() + code.len() / 4
        } else {
            0
        },
    );
    if want_code {
        line = line.str("code", &code);
    }
    line = line.rules(&rules);
    if map_mode != MapMode::Off {
        line = line.raw("map", map.as_deref().unwrap_or("null"));
    }
    if let Some(key) = &request.key {
        let (generation, changed) = register(&shared.registry, key, rules);
        line = line
            .value("generation", &generation)
            .value("rulesChanged", &changed);
    }
    line.finish()
}

fn set_file_rules(request: &Request, id: &str, shared: &Shared) -> String {
    let (Some(key), Some(tuples)) = (&request.key, &request.rules) else {
        return input_error_line(id, "setFileRules needs key and rules");
    };
    let mut rules = Vec::with_capacity(tuples.len());
    for (i, tuple) in tuples.iter().enumerate() {
        match StylexRule::from_metadata_tuple(tuple) {
            Ok(rule) => rules.push(rule),
            Err(detail) => {
                return protocol_error_line(id, "BAD_RULE", &format!("rule {i}: {detail}"));
            }
        }
    }
    let (generation, changed) = register(&shared.registry, key, rules);
    OkLine::new(id, 0)
        .value("generation", &generation)
        .value("rulesChanged", &changed)
        .finish()
}

fn remove_file(request: &Request, id: &str, shared: &Shared) -> String {
    let Some(key) = &request.key else {
        return input_error_line(id, "removeFile needs key");
    };
    let mut registry = lock(&shared.registry);
    let removed = registry.remove_file(key);
    OkLine::new(id, 0)
        .value("removed", &removed)
        .value("generation", &registry.generation())
        .finish()
}

fn sheet(request: &Request, id: &str, shared: &Shared) -> String {
    let cfg = match AssembleConfig::from_json(request.config.as_ref().unwrap_or(&Value::Null)) {
        Ok(cfg) => cfg,
        Err(err) => return protocol_error_line(id, "BAD_CONFIG", &err.to_string()),
    };
    let mut registry = lock(&shared.registry);
    match registry.emit(&cfg) {
        Ok(css) => OkLine::new(id, css.len())
            .str("css", &css)
            .value("generation", &registry.generation())
            .value("files", &registry.file_count())
            .finish(),
        Err(err) => protocol_error_line(id, "ASSEMBLE", &err.to_string()),
    }
}

// The boot pass: walk `roots` on this daemon's own threads, compile every
// StyleX-bearing file once, and register its rules under its absolute path.
fn warm(request: &Request, id: &str, shared: &Shared) -> String {
    let (Some(root), Some(roots)) = (&request.root, &request.roots) else {
        return input_error_line(id, "warm needs root and roots");
    };
    let raw_options = match &request.options {
        Some(raw) => match serde_json::from_str::<Value>(raw.get()) {
            Ok(value) => value,
            Err(err) => return protocol_error_line(id, "BAD_OPTIONS", &err.to_string()),
        },
        None => Value::Null,
    };
    let options = match CompilerOptions::from_json(&raw_options).and_then(|o| o.resolve()) {
        Ok(options) => options,
        Err(error) => return protocol_error_line(id, "BAD_OPTIONS", &error.message),
    };
    let started = Instant::now();
    let files = compile::discover(roots, shared.threads);
    let next = AtomicUsize::new(0);
    let compiled = AtomicUsize::new(0);
    let errors: Mutex<Vec<Value>> = Mutex::new(Vec::new());
    let cwd = Path::new(root.trim_end_matches('/'));
    let fs = MemoFs::snapshot();
    let fs = &fs;
    std::thread::scope(|scope| {
        for _ in 0..shared.threads {
            let (files, next, compiled, errors, options) =
                (&files, &next, &compiled, &errors, &options);
            scope.spawn(move || {
                let mut allocator = Allocator::default();
                let mut buf = Vec::new();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(path) = files.get(i) else { break };
                    allocator.reset();
                    let source = match compile::read_source(path, &mut buf) {
                        Ok(source) => source,
                        Err(err) => {
                            let detail = json!({ "file": path, "message": err.to_string() });
                            errors
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .push(detail);
                            continue;
                        }
                    };
                    if !might_contain_stylex(source, options) {
                        continue;
                    }
                    let ctx = FileContext {
                        filename: path,
                        source_text: source,
                        cwd,
                    };
                    match transform_source_in(&allocator, &ctx, options, fs) {
                        Ok(Some(result)) if !result.rules.is_empty() => {
                            compiled.fetch_add(1, Ordering::Relaxed);
                            let key = path.to_string_lossy().replace('\\', "/");
                            register(&shared.registry, &key, result.rules);
                        }
                        Ok(_) => {}
                        Err(error) => {
                            let detail = json!({ "file": path, "message": error.message });
                            errors
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .push(detail);
                        }
                    }
                }
            });
        }
    });
    let errors = errors.into_inner().unwrap_or_else(PoisonError::into_inner);
    let generation = lock(&shared.registry).generation();
    OkLine::new(id, 0)
        .value("files", &files.len())
        .value("stylexFiles", &compiled.load(Ordering::Relaxed))
        .value("errors", &errors)
        .value("generation", &generation)
        .value("wallMs", &(started.elapsed().as_secs_f64() * 1e3))
        .finish()
}
