//! Env-gated stage timers (`STYLEX_STAGE_TIMINGS=1`): outermost-only per
//! stage, so re-entrant paths (nested eval, cross-file parse) never double-count.

use std::cell::RefCell;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

#[derive(Clone, Copy)]
pub enum Stage {
    NdjsonIn = 0,
    Options,
    Parse,
    ImportScan,
    Semantic,
    Transform,
    Eval,
    Create,
    PrintSplice,
    ApplyPlan,
    PrintAst,
    NdjsonOut,
    JobTotal,
    CacheRead,
    CacheParse,
    CacheReplay,
    CacheDecode,
    Fs,
    AssembleConsts,
    AssembleDedupe,
    AssembleKeys,
    AssembleSort,
    AssembleSubst,
    AssembleRender,
    CreateMiss,
}

const STAGE_COUNT: usize = 25;
const NAMES: [&str; STAGE_COUNT] = [
    "ndjson_in",
    "options",
    "parse",
    "import_scan",
    "semantic",
    "transform",
    "eval",
    "create",
    "print_splice",
    "apply_plan",
    "print_ast",
    "ndjson_out",
    "job_total",
    "cache_read",
    "cache_parse",
    "cache_replay",
    "cache_decode",
    "fs",
    "assemble_consts",
    "assemble_dedupe",
    "assemble_keys",
    "assemble_sort",
    "assemble_subst",
    "assemble_render",
    "create_miss",
];

static TOTAL_NS: [AtomicU64; STAGE_COUNT] = [const { AtomicU64::new(0) }; STAGE_COUNT];
static COUNTS: [AtomicU64; STAGE_COUNT] = [const { AtomicU64::new(0) }; STAGE_COUNT];

#[inline]
pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("STYLEX_STAGE_TIMINGS").is_ok_and(|v| v == "1"))
}

thread_local! {
    static DEPTH: RefCell<[u32; STAGE_COUNT]> = const { RefCell::new([0; STAGE_COUNT]) };
}

pub struct StageGuard {
    stage: usize,
    start: Option<Instant>,
}

/// Hold the returned guard for the span of the stage; `None`/nested guards
/// are inert. Disabled cost: one lazy-static bool load and branch.
#[inline]
#[must_use]
pub fn start(stage: Stage) -> Option<StageGuard> {
    if !enabled() {
        return None;
    }
    let stage = stage as usize;
    let outermost = DEPTH.with(|d| {
        let mut d = d.borrow_mut();
        d[stage] += 1;
        d[stage] == 1
    });
    Some(StageGuard {
        stage,
        start: outermost.then(Instant::now),
    })
}

impl Drop for StageGuard {
    fn drop(&mut self) {
        DEPTH.with(|d| d.borrow_mut()[self.stage] -= 1);
        if let Some(start) = self.start {
            let ns = start.elapsed().as_nanos() as u64;
            TOTAL_NS[self.stage].fetch_add(ns, Ordering::Relaxed);
            COUNTS[self.stage].fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// One `{"stageTimings": …}` JSON line to stderr; no-op when disabled.
pub fn report_to_stderr() {
    if !enabled() {
        return;
    }
    let mut body = String::from("{\"stageTimings\":{");
    for (i, name) in NAMES.iter().enumerate() {
        if i > 0 {
            body.push(',');
        }
        let total = TOTAL_NS[i].load(Ordering::Relaxed);
        let count = COUNTS[i].load(Ordering::Relaxed);
        body.push_str(&format!(
            "\"{name}\":{{\"total_ms\":{:.3},\"count\":{count}}}",
            total as f64 / 1e6
        ));
    }
    body.push_str("}}");
    eprintln!("{body}");
}
