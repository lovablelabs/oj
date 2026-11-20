//! `serve --stdio`: a resident NDJSON request loop with an in-process [`RuleRegistry`]
//! for the Vite dev plugin; responses correlate by `id`, never by order (docs/serve-protocol.md).

use std::borrow::Cow;
use std::io::{BufRead, BufReader, IoSlice, StdoutLock, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError, mpsc};
use std::time::Instant;

use crate::api::{
    FileContext, MapMode, might_contain_stylex, transform_source_in_with_map,
    transform_source_rules_in,
};
use crate::assemble::{AssembleConfig, RuleRegistry};
use crate::module_resolution::MemoFs;
use crate::options::CompilerOptions;
use crate::rules::StylexRule;
use crate::timings::{self, Stage};
use oxc_allocator::Allocator;
use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::{Value, json};

use crate::cli::batch::{ARENA, read_job_source, resolve_options_memoized, write_all_vectored};
use crate::cli::compile;
use crate::cli::queue::{Bounds, JobQueue};
use crate::cli::wire::{OkLine, compile_error_line, input_error_line, protocol_error_line};

// A worker keeps its arena between requests; one that grew for a large file is
// dropped so a long dev session does not pin every thread's high-water mark.
const ARENA_CAP_BYTES: usize = 4 << 20;

// A pipe hands over at most 64 KiB a read; the buffer takes that whole, so a
// request line is usually copied out of it once, at its final size.
const STDIN_BUFFER_BYTES: usize = 256 << 10;

// What one rule adds to a reply line, for sizing the line up front.
const RULE_LINE_BYTES: usize = 96;

/// A request string: borrowed from the line unless it carries escapes.
#[derive(Deserialize)]
struct Text<'a>(#[serde(borrow)] Cow<'a, str>);

impl std::ops::Deref for Text<'_> {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Request<'a> {
    #[serde(default, borrow)]
    id: Option<&'a RawValue>,
    #[serde(default, borrow)]
    op: Option<Text<'a>>,
    #[serde(default, borrow)]
    root: Option<Text<'a>>,
    #[serde(default, borrow)]
    filename: Option<Text<'a>>,
    #[serde(default, borrow)]
    source: Option<Text<'a>>,
    #[serde(default, borrow)]
    source_path: Option<Text<'a>>,
    #[serde(default, borrow)]
    options: Option<&'a RawValue>,
    #[serde(default, borrow)]
    key: Option<Text<'a>>,
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

/// One reply as written: `line` ends in its newline.
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
    let queue: JobQueue<Vec<u8>> = JobQueue::new(Bounds::for_threads(threads));
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
                    response.line.push('\n');
                    if tx.send((i, response)).is_err() {
                        queue.abort();
                        break;
                    }
                }
            });
        }
        drop(tx);
        scope.spawn(move || write_responses(&rx));
        read_requests(queue);
        queue.finish();
    });
    timings::report_to_stderr();
}

// Lines stay raw bytes: the worker that parses a request is also the one that
// validates its text. Requests that arrived together are published together,
// one queue turn for the burst; nothing is held back across a read that could wait.
fn read_requests(queue: &JobQueue<Vec<u8>>) {
    let stdin = std::io::stdin();
    let mut input = BufReader::with_capacity(STDIN_BUFFER_BYTES, stdin.lock());
    let bounds = queue.bounds();
    let mut block = Vec::new();
    let mut block_bytes = 0usize;
    loop {
        let another_buffered = memchr::memchr(b'\n', input.buffer()).is_some();
        let full = block.len() >= bounds.block_lines || block_bytes >= bounds.block_bytes;
        if !block.is_empty() && (full || !another_buffered) {
            block_bytes = 0;
            if !queue.publish(&mut block) {
                return;
            }
        }
        let mut line = Vec::new();
        match input.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(err) => {
                eprintln!("{}", json!({ "error": "IO", "detail": err.to_string() }));
                std::process::exit(1);
            }
        }
        if line.ends_with(b"\n") {
            line.pop();
            if line.ends_with(b"\r") {
                line.pop();
            }
        }
        if !is_blank(&line) {
            block_bytes += line.len();
            block.push(line);
        }
    }
    if !block.is_empty() {
        queue.publish(&mut block);
    }
}

/// `str::trim().is_empty()` for a raw line; only a line with no ASCII text in
/// front pays for decoding.
fn is_blank(line: &[u8]) -> bool {
    match line
        .iter()
        .position(|byte| !matches!(byte, b' ' | b'\t'..=b'\r'))
    {
        None => true,
        Some(at) if line[at].is_ascii() => false,
        Some(_) => std::str::from_utf8(line).is_ok_and(|text| text.trim().is_empty()),
    }
}

// Finished responses go out together in one vectored write (an idle daemon
// writes at once); a shutdown is answered only after every request read
// before it, then exits.
fn write_responses(rx: &mpsc::Receiver<(usize, Response)>) {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut ready: Vec<String> = Vec::new();
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
            ready.push(response.line);
        }
        let exiting = shutdown.as_ref().is_some_and(|(at, _)| done_below > *at);
        if exiting && let Some((_, line)) = shutdown.take() {
            ready.push(line);
        }
        write_lines(&mut out, &mut ready);
        if exiting {
            timings::report_to_stderr();
            std::process::exit(0);
        }
    }
}

// Every line ends in its newline, so std's stdout `LineWriter` hands the whole
// batch to the fd: one syscall, and no copy into a buffer on the way.
fn write_lines(out: &mut StdoutLock<'_>, lines: &mut Vec<String>) {
    let written = match lines.as_slice() {
        [] => return,
        [line] => out.write_all(line.as_bytes()),
        lines => {
            let mut slices: Vec<IoSlice<'_>> = lines
                .iter()
                .map(|line| IoSlice::new(line.as_bytes()))
                .collect();
            write_all_vectored(out, &mut slices)
        }
    };
    if written.and_then(|()| out.flush()).is_err() {
        std::process::exit(1);
    }
    lines.clear();
}

fn attach_wall(line: &mut String, wall_ns: u64) {
    debug_assert!(line.ends_with('}'));
    line.pop();
    line.push_str(&format!(",\"wall_ns\":{wall_ns}}}"));
}

fn handle(line: &[u8], shared: &Shared) -> Response {
    let _t = timings::start(Stage::JobTotal);
    let request = {
        let _t = timings::start(Stage::NdjsonIn);
        serde_json::from_slice::<Request<'_>>(line)
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
    let id = request.id.map_or(Cow::Borrowed("null"), reply_id);
    let id = &*id;
    let line = match request.op.as_deref().unwrap_or("transform") {
        "transform" => transform(&request, id, shared),
        "setFileRules" => set_file_rules(&request, id, shared),
        "removeFile" => remove_file(&request, id, shared),
        "sheet" => sheet(&request, id, shared),
        "warm" => warm(&request, id, shared),
        "hello" => OkLine::new(id, 0)
            .str("fingerprint", crate::cache::compiler_fingerprint())
            .value("threads", &shared.threads)
            .value("pid", &std::process::id())
            .finish(),
        "shutdown" => {
            return Response {
                line: OkLine::new(id, 0).finish(),
                shutdown: true,
            };
        }
        other => protocol_error_line(id, "BAD_OP", &format!("unknown op {other}")),
    };
    Response {
        line,
        shutdown: false,
    }
}

/// The id as serde_json would re-serialize it. Ids already in that form
/// (small integers, strings without escapes, literals) are echoed from the
/// request's own bytes; anything else takes the `Value` round trip.
fn reply_id(raw: &RawValue) -> Cow<'_, str> {
    let text = raw.get();
    let bytes = text.as_bytes();
    let canonical = match bytes {
        [b'"', ..] => !bytes.contains(&b'\\'),
        [b'0'] | [b'1'..=b'9', ..] | [b'-', b'1'..=b'9', ..] => {
            bytes.len() <= 18 && bytes[1..].iter().all(u8::is_ascii_digit)
        }
        _ => matches!(text, "null" | "true" | "false"),
    };
    if canonical {
        return Cow::Borrowed(text);
    }
    let value: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    Cow::Owned(value.to_string())
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
fn transform(request: &Request<'_>, id: &str, shared: &Shared) -> String {
    let (Some(root), Some(filename)) = (request.root.as_deref(), request.filename.as_deref())
    else {
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
        match resolve_options_memoized(request.options) {
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
    let want_code = request.want_code != Some(false);
    // Neither the code nor a map of it is read: only the rules are compiled.
    let rules_only = !want_code && map_mode == MapMode::Off;
    let compiled = ARENA.with(|cell| {
        let mut allocator = cell.borrow_mut();
        allocator.reset();
        let fs = MemoFs::shared().session();
        let result = if rules_only {
            transform_source_rules_in(&allocator, &ctx, &options, &fs).map(|result| {
                let rules = result.map_or_else(Vec::new, |result| result.rules);
                (Cow::Borrowed(&*source), rules, None)
            })
        } else {
            transform_source_in_with_map(&allocator, &ctx, &options, &fs, map_mode).map(|result| {
                match result {
                    Some(result) => (Cow::Owned(result.code), result.rules, result.map),
                    None => (Cow::Borrowed(&*source), Vec::new(), None),
                }
            })
        };
        if allocator.capacity() > ARENA_CAP_BYTES {
            *allocator = Allocator::default();
        }
        result
    });
    let (code, rules, map) = match compiled {
        Ok(compiled) => compiled,
        Err(error) => return compile_error_line(id, root, filename, error),
    };
    let _t = timings::start(Stage::NdjsonOut);
    let code_bytes = if want_code {
        code.len() + code.len() / 8
    } else {
        0
    };
    let mut line = OkLine::new(id, code_bytes + rules.len() * RULE_LINE_BYTES);
    if want_code {
        line = line.str("code", &code);
    }
    line = line.rules(&rules);
    if map_mode != MapMode::Off {
        line = line.raw("map", map.as_deref().unwrap_or("null"));
    }
    if let Some(key) = request.key.as_deref() {
        let (generation, changed) = register(&shared.registry, key, rules);
        line = line
            .value("generation", &generation)
            .value("rulesChanged", &changed);
    }
    line.finish()
}

fn set_file_rules(request: &Request<'_>, id: &str, shared: &Shared) -> String {
    let (Some(key), Some(tuples)) = (request.key.as_deref(), &request.rules) else {
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

fn remove_file(request: &Request<'_>, id: &str, shared: &Shared) -> String {
    let Some(key) = request.key.as_deref() else {
        return input_error_line(id, "removeFile needs key");
    };
    let mut registry = lock(&shared.registry);
    let removed = registry.remove_file(key);
    OkLine::new(id, 0)
        .value("removed", &removed)
        .value("generation", &registry.generation())
        .finish()
}

fn sheet(request: &Request<'_>, id: &str, shared: &Shared) -> String {
    let cfg = match AssembleConfig::from_json(request.config.as_ref().unwrap_or(&Value::Null)) {
        Ok(cfg) => cfg,
        Err(err) => return protocol_error_line(id, "BAD_CONFIG", &err.to_string()),
    };
    // The lock covers the assembly alone: transforms go on registering while
    // the sheet is escaped into its reply.
    let (css, generation, files) = {
        let mut registry = lock(&shared.registry);
        (
            registry.emit(&cfg),
            registry.generation(),
            registry.file_count(),
        )
    };
    match css {
        Ok(css) => OkLine::new(id, css.len() + css.len() / 16)
            .str("css", &css)
            .value("generation", &generation)
            .value("files", &files)
            .finish(),
        Err(err) => protocol_error_line(id, "ASSEMBLE", &err.to_string()),
    }
}

// The boot pass: walk `roots` on this daemon's own threads, compile every
// StyleX-bearing file once, and register its rules under its absolute path.
// Each compile is a session of the live fs `transform` compiles through, so
// the transforms that follow find what the pass resolved already kept.
fn warm(request: &Request<'_>, id: &str, shared: &Shared) -> String {
    let (Some(root), Some(roots)) = (request.root.as_deref(), &request.roots) else {
        return input_error_line(id, "warm needs root and roots");
    };
    let raw_options = match request.options {
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
                    let fs = MemoFs::shared().session();
                    match transform_source_rules_in(&allocator, &ctx, options, &fs) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warm_keeps_what_it_resolved_for_the_transforms_that_follow() {
        let root = std::env::temp_dir().join(format!("fru-serve-warm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        let root = std::fs::canonicalize(&root).unwrap();
        std::fs::write(
            root.join("src/tokens.stylex.ts"),
            "import * as stylex from '@stylexjs/stylex';\nexport const vars = stylex.defineVars({ c: 'red' });\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/box.ts"),
            "import * as stylex from '@stylexjs/stylex';\nimport { vars } from './tokens.stylex';\nexport const s = stylex.create({ a: { color: vars.c } });\n",
        )
        .unwrap();
        let src = root.join("src");
        assert!(!MemoFs::shared().keeps_relative(&src, "./tokens.stylex"));

        let line = json!({
            "op": "warm",
            "root": root,
            "roots": [src],
            "options": { "unstable_moduleResolution": { "type": "commonJS", "rootDir": root } },
        })
        .to_string();
        let request: Request = serde_json::from_str(&line).unwrap();
        let shared = Shared {
            registry: Mutex::new(RuleRegistry::new()),
            threads: 1,
        };
        let response = warm(&request, "1", &shared);
        assert!(response.contains(r#""stylexFiles":2"#), "{response}");
        assert!(response.contains(r#""errors":[]"#), "{response}");
        assert!(MemoFs::shared().keeps_relative(&src, "./tokens.stylex"));
    }

    #[test]
    fn reply_ids_match_a_value_round_trip() {
        for id in [
            "0",
            "7",
            "-7",
            "-0",
            "007e0",
            "1.0",
            "1e2",
            "123456789012345678",
            "1234567890123456789",
            "18446744073709551616",
            "null",
            "true",
            "false",
            r#""plain""#,
            r#""ünï""#,
            r#""esc\u0041\n""#,
            r#"{"a": [1, "b"]}"#,
            "[ 1 ,2 ]",
        ] {
            let Ok(raw) = serde_json::from_str::<&RawValue>(id) else {
                continue;
            };
            let expected = serde_json::from_str::<Value>(id).unwrap().to_string();
            assert_eq!(reply_id(raw), expected, "id {id}");
        }
    }

    #[test]
    fn blank_lines_are_what_trim_calls_empty() {
        for line in [
            &b""[..],
            b" ",
            b"\t \x0b\x0c\r",
            b"{}",
            b"  {}",
            "\u{a0}\u{2003}".as_bytes(),
            "\u{a0}x".as_bytes(),
            b" \xff",
            b"\xff",
        ] {
            let expected = std::str::from_utf8(line).is_ok_and(|text| text.trim().is_empty());
            assert_eq!(is_blank(line), expected, "line {line:?}");
        }
    }
}
