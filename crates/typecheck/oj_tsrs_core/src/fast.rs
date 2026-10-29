//! Opt-in performance defaults shared by the CLI and compiler harnesses.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

// 0: environment default, 1: explicitly enabled, 2: explicitly disabled.
static MODE: AtomicU8 = AtomicU8::new(0);

/// Set before creating programs or checker threads. Explicit feature options take precedence.
pub fn set_enabled(enabled: bool) {
    // Relaxed: this is a configuration flag, not a publication barrier.
    MODE.store(if enabled { 1 } else { 2 }, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    static ENV: OnceLock<bool> = OnceLock::new();
    // Relaxed: see `set_enabled`; workers start after configuration.
    match MODE.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => *ENV.get_or_init(|| std::env::var("TSRS_FAST").is_ok_and(|v| v == "1")),
    }
}
