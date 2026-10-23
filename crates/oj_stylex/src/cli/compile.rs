//! `compile`: walk a project, compile every StyleX-bearing file, emit one CSS
//! sheet — the whole batch+assemble pipeline behind a single command.

use std::collections::{BTreeMap, HashSet};
use std::fs::DirEntry;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

use crate::api::{FileContext, might_contain_stylex, transform_source_in};
use crate::assemble::{AssembleConfig, assemble};
use crate::module_resolution::MemoFs;
use crate::options::CompilerOptions;
use crate::rules::StylexRule;
use serde_json::{Value, json};

// None = pre-gated (no StyleX needle); the walk index keys in-order draining.
// The bool is the cache-hit flag (always false with no cache configured).
type FileOutcome = Option<Result<(PathBuf, Vec<StylexRule>, bool), String>>;

/// Content identity of a rule for the cross-file dedupe in the drain loop.
type RuleKeySet = HashSet<(
    std::sync::Arc<str>,
    std::sync::Arc<str>,
    Option<std::sync::Arc<str>>,
    u64,
)>;

const EXTENSIONS: [&[u8]; 6] = [b"ts", b"tsx", b"js", b"jsx", b"mjs", b"cjs"];
const SKIP_DIRS: [&[u8]; 4] = [b"node_modules", b"dist", b"build", b"target"];
// Ambient declarations admit no initializers, so these can never emit rules.
const DECLARATION_SUFFIXES: [&[u8]; 3] = [b".d.ts", b".d.mts", b".d.cts"];

fn usage_error(detail: String) -> ! {
    eprintln!("{}", json!({ "error": "BAD_ARGS", "detail": detail }));
    std::process::exit(64);
}

fn is_candidate(name: &[u8]) -> bool {
    let Some(dot) = name.iter().rposition(|&b| b == b'.') else {
        return false;
    };
    EXTENSIONS.contains(&&name[dot + 1..])
        && !DECLARATION_SUFFIXES.iter().any(|s| name.ends_with(s))
}

// readdir's d_type answers for plain entries; symlinks (and filesystems
// without d_type) still need the stat so they resolve like `Path::is_dir`.
fn is_dir(entry: &DirEntry) -> bool {
    match entry.file_type() {
        Ok(kind) if !kind.is_symlink() => kind.is_dir(),
        _ => entry.path().is_dir(),
    }
}

struct DirQueueState {
    pending: Vec<PathBuf>,
    in_flight: usize,
}

struct DirQueue {
    state: Mutex<DirQueueState>,
    ready: Condvar,
}

impl DirQueue {
    fn take(&self) -> Option<PathBuf> {
        let mut state = self.state.lock().unwrap();
        loop {
            if let Some(dir) = state.pending.pop() {
                state.in_flight += 1;
                return Some(dir);
            }
            if state.in_flight == 0 {
                return None;
            }
            state = self.ready.wait(state).unwrap();
        }
    }

    fn finish(&self, subdirs: Vec<PathBuf>) {
        let mut state = self.state.lock().unwrap();
        state.in_flight -= 1;
        if !subdirs.is_empty() || state.in_flight == 0 {
            state.pending.extend(subdirs);
            self.ready.notify_all();
        }
    }
}

fn walk_worker(queue: &DirQueue, files: &mut Vec<PathBuf>) {
    while let Some(dir) = queue.take() {
        let mut subdirs = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.as_encoded_bytes();
                if name.starts_with(b".") {
                    continue;
                }
                if is_dir(&entry) {
                    if !SKIP_DIRS.contains(&name) {
                        subdirs.push(entry.path());
                    }
                } else if is_candidate(name) {
                    files.push(entry.path());
                }
            }
        }
        queue.finish(subdirs);
    }
}

/// Every candidate file under `roots`, sorted so the compile drain order (and
/// with it const last-wins) never depends on how the walk was scheduled.
pub fn discover(roots: &[PathBuf], threads: usize) -> Vec<PathBuf> {
    let queue = DirQueue {
        state: Mutex::new(DirQueueState {
            pending: roots.to_vec(),
            in_flight: 0,
        }),
        ready: Condvar::new(),
    };
    let mut files: Vec<PathBuf> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut files = Vec::new();
                    walk_worker(&queue, &mut files);
                    files
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|w| w.join().expect("walker panicked"))
            .collect()
    });
    files.sort();
    files
}

pub fn read_source<'a>(path: &Path, buf: &'a mut Vec<u8>) -> std::io::Result<&'a str> {
    buf.clear();
    std::fs::File::open(path)?.read_to_end(buf)?;
    // `read_to_string`'s wording, so the reported error is unchanged.
    std::str::from_utf8(buf).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "stream did not contain valid UTF-8",
        )
    })
}

pub fn run(
    root: &Path,
    options_json: Option<&str>,
    out: Option<&Path>,
    threads: usize,
    cache_dir: Option<&Path>,
) {
    let root = match std::fs::canonicalize(root) {
        Ok(root) => root,
        Err(err) => usage_error(format!("cannot open {}: {err}", root.display())),
    };
    let raw_options: Value = match options_json {
        Some(text) => match serde_json::from_str(text) {
            Ok(v) => v,
            Err(err) => usage_error(format!("--options is not valid JSON: {err}")),
        },
        // The pilot default: cross-file theme imports resolve against the root.
        None => json!({ "unstable_moduleResolution": { "type": "commonJS", "rootDir": root } }),
    };
    let options = match CompilerOptions::from_json(&raw_options).and_then(|o| o.resolve()) {
        Ok(options) => options,
        Err(err) => usage_error(err.message),
    };
    // --cache-dir wins; STYLEX_CACHE_DIR is the fallback; STYLEX_CACHE=0 kills.
    let cache_dir = cache_dir
        .map(Path::to_path_buf)
        .or_else(|| oj_env::get().knobs.stylex_cache_dir.clone())
        .filter(|_| !oj_env::get().knobs.stylex_cache_off);
    let cache = cache_dir.map(|dir| match crate::cache::CacheStore::new(&dir) {
        Ok(store) => store,
        Err(err) => usage_error(format!("cannot open cache dir {}: {err}", dir.display())),
    });

    let files = discover(std::slice::from_ref(&root), threads.max(1));

    let next = AtomicUsize::new(0);
    let (tx, rx) = std::sync::mpsc::sync_channel::<(usize, FileOutcome)>(threads.max(1) * 4);
    let mut rules: Vec<StylexRule> = Vec::new();
    let mut seen: RuleKeySet = HashSet::new();
    let (mut compiled, mut errors) = (0usize, 0usize);
    let (mut cache_hits, mut cache_misses) = (0usize, 0usize);
    let fs = MemoFs::snapshot();
    std::thread::scope(|scope| {
        let (files, next, options, root, cache, fs) = (&files, &next, &options, &root, &cache, &fs);
        for _ in 0..threads.max(1) {
            let tx = tx.clone();
            scope.spawn(move || {
                let mut allocator = oxc_allocator::Allocator::default();
                let mut buf = Vec::new();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(path) = files.get(i) else { break };
                    allocator.reset();
                    let outcome: FileOutcome = match read_source(path, &mut buf) {
                        Err(err) => Some(Err(format!("{}: {err}", path.display()))),
                        Ok(source) if !might_contain_stylex(source, options) => None,
                        Ok(source) => {
                            let ctx = FileContext {
                                filename: path,
                                source_text: source,
                                cwd: root,
                            };
                            match cache {
                                Some(store) => match crate::cache::compile_through_cache_in(
                                    &allocator, store, &ctx, options, fs, false,
                                ) {
                                    Ok((result, status)) => {
                                        let hit = matches!(status, crate::cache::CacheStatus::Hit);
                                        let rules = result.map(|r| r.rules).unwrap_or_default();
                                        Some(Ok((path.clone(), rules, hit)))
                                    }
                                    Err(err) => {
                                        Some(Err(format!("{}: {}", path.display(), err.message)))
                                    }
                                },
                                None => match transform_source_in(&allocator, &ctx, options, fs) {
                                    Ok(Some(result)) => {
                                        Some(Ok((path.clone(), result.rules, false)))
                                    }
                                    Ok(None) => Some(Ok((path.clone(), Vec::new(), false))),
                                    Err(err) => {
                                        Some(Err(format!("{}: {}", path.display(), err.message)))
                                    }
                                },
                            }
                        }
                    };
                    if tx.send((i, outcome)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);

        // In-order drain keyed by walk index: deterministic output, rules
        // deduped on arrival instead of held per-file until the join.
        let mut pending: BTreeMap<usize, FileOutcome> = BTreeMap::new();
        let mut next_emit = 0usize;
        for (i, outcome) in rx {
            pending.insert(i, outcome);
            while let Some(outcome) = pending.remove(&next_emit) {
                next_emit += 1;
                match outcome {
                    None => {}
                    Some(Err(message)) => {
                        errors += 1;
                        eprintln!("{}", json!({ "error": "COMPILE", "detail": message }));
                    }
                    Some(Ok((_, file_rules, hit))) => {
                        if hit {
                            cache_hits += 1;
                        } else {
                            cache_misses += 1;
                        }
                        if !file_rules.is_empty() {
                            compiled += 1;
                        }
                        for rule in file_rules {
                            // Consts resolve last-wins by value at assembly: keep every copy.
                            // Plain duplicates collapse; assemble's content sort makes it neutral.
                            let constish = rule.const_key.is_some() || rule.const_val.is_some();
                            if constish
                                || seen.insert((
                                    rule.class_name.clone(),
                                    rule.ltr.clone(),
                                    rule.rtl.clone(),
                                    rule.priority.to_bits(),
                                ))
                            {
                                rules.push(rule);
                            }
                        }
                    }
                }
            }
        }
    });

    let cfg = AssembleConfig::from_json(&Value::Null).expect("default assemble config");
    let css = match assemble(&rules, &cfg) {
        Ok(css) => css,
        Err(err) => usage_error(format!("assemble failed: {err}")),
    };
    match out {
        Some(path) => {
            if let Err(err) = std::fs::write(path, &css) {
                usage_error(format!("cannot write {}: {err}", path.display()));
            }
        }
        None => {
            std::io::stdout()
                .write_all(css.as_bytes())
                .expect("stdout write");
        }
    }
    let mut summary = json!({
        "files": files.len(),
        "stylexFiles": compiled,
        "rules": rules.len(),
        "cssBytes": css.len(),
        "errors": errors,
    });
    if cache.is_some() {
        summary["cache"] = json!({ "hits": cache_hits, "misses": cache_misses });
    }
    eprintln!("{summary}");
    if errors > 0 {
        std::process::exit(1);
    }
}
