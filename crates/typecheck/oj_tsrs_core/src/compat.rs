//! Go-compatible check history (notes/perf-order-independence.md).
//!
//! A few checker caches in typescript-go let the printed output depend on what the checker happened to see first,
//! so changing checker count or file order can change diagnostics in TypeScript itself. The default and
//! `--checkerAssignment go` preserve that upstream behavior. `--fast` ("canonical") removes selected history
//! dependencies; other upstream cases remain (notes/open-history-dependence.md). For byte-identity with tsgo,
//! compare the same inputs, checker count and file order. The conformance, fourslash and tsc harnesses use Go history,
//! because their baselines are tsgo's output with one checker in program order. Every place that has the two
//! behaviours asks `go_compatible_history()`.

use std::sync::atomic::{AtomicU8, Ordering};

const UNSET: u8 = 0;
const CANONICAL: u8 = 1;
const GO: u8 = 2;

static MODE: AtomicU8 = AtomicU8::new(UNSET);

/// Whether checker caches keep typescript-go's history-dependent behaviour. Explicit assignments override `--fast`.
pub fn go_compatible_history() -> bool {
    // A plain flag set before any checker exists; nothing is published through it.
    match MODE.load(Ordering::Relaxed) {
        UNSET => {
            let go = std::env::var("TSRS_CHECKER_ASSIGNMENT")
                .ok()
                .filter(|v| !v.is_empty())
                .map_or_else(|| !crate::fast::enabled(), |v| v == "go");
            // Racing first readers see the same configuration, set before any checker starts.
            MODE.store(if go { GO } else { CANONICAL }, Ordering::Relaxed);
            go
        }
        mode => mode == GO,
    }
}

/// Sets the mode (the `--checkerAssignment` option). Call before creating checkers.
pub fn set_go_compatible_history(on: bool) {
    // See `go_compatible_history`.
    MODE.store(if on { GO } else { CANONICAL }, Ordering::Relaxed);
}

/// Selects the checker history required by tsgo reference baselines.
///
/// For harnesses whose baselines are tsgo's output: Go's behaviour unless `TSRS_CHECKER_ASSIGNMENT` names
/// another assignment, `TSRS_HISTORY=canonical` or `TSRS_FAST=1` (then the canonical behaviour, to measure how
/// the baselines differ from it). Fast-mode scheduling includes stealing without a named assignment.
/// Explicit Go assignment/history overrides the fast preset's history.
pub fn use_go_history_for_tsgo_baselines() {
    let assignment = std::env::var("TSRS_CHECKER_ASSIGNMENT").unwrap_or_default();
    let history = std::env::var("TSRS_HISTORY").unwrap_or_default();
    let canonical = !assignment.is_empty() && assignment != "go" || history == "canonical";
    let fast = canonical || crate::fast::enabled();
    crate::fast::set_enabled(fast);
    set_go_compatible_history(!canonical && (assignment == "go" || history == "go" || !fast));
}
