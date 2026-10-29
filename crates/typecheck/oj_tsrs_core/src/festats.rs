//! Attributes parallel program-construction time to frontend phases.
//!
//! tsrs-only, env-gated (`TSRS_FRONTEND_STATS=1`): per-thread time accumulators for program construction's parallel parse + resolve phase, printed after `--extendedDiagnostics` by the loader (filesparser.rs).
//!
//!
//! Off, each probe is one atomic load and a branch; on, two `Instant::now()` calls per probe on the
//! calling thread's own counters (no shared writes), summed over the pool once per round.

use std::cell::Cell;
use std::sync::OnceLock;
use std::time::Instant;

#[derive(Clone, Copy)]
#[repr(usize)]
pub enum Cat {
    /// Whole prefetch job (metadata + read + parse + bind + resolve + speculative spawns).
    JobWall,
    Meta,
    Read,
    Parse,
    Bind,
    Resolve,
    /// Waiting for the speculative walk's claimed-set mutex.
    LockClaim,
    /// Waiting for a resolver / package.json `SyncMap` lock.
    LockSyncMap,
    /// Waiting for a cached-vfs lock.
    LockVfs,
    /// Lock acquisitions (counts): claimed set, `SyncMap`s, cached vfs.
    ClaimOps,
    SyncMapOps,
    VfsOps,
    /// Number of jobs (a count, not nanoseconds).
    Jobs,
    /// The longest job (nanoseconds; a maximum, not a sum).
    JobMax,
}

pub const CATS: usize = Cat::JobMax as usize + 1;

/// Whether the category is a count rather than nanoseconds.
pub fn is_count(cat: usize) -> bool {
    cat >= Cat::ClaimOps as usize && cat <= Cat::Jobs as usize
}

/// Whether the category combines by maximum rather than by sum.
pub fn is_max(cat: usize) -> bool {
    cat == Cat::JobMax as usize
}

static ENABLED: OnceLock<bool> = OnceLock::new();

#[inline]
pub fn enabled() -> bool {
    *ENABLED.get_or_init(|| std::env::var_os("TSRS_FRONTEND_STATS").is_some())
}

thread_local! {
    static ACC: Cell<[u64; CATS]> = const { Cell::new([0; CATS]) };
}

#[inline]
pub fn add(cat: Cat, n: u64) {
    ACC.with(|a| {
        let mut v = a.get();
        v[cat as usize] += n;
        a.set(v);
    });
}

/// Raises `cat` to `n` if larger (maximum categories).
#[inline]
pub fn add_max(cat: Cat, n: u64) {
    ACC.with(|a| {
        let mut v = a.get();
        v[cat as usize] = v[cat as usize].max(n);
        a.set(v);
    });
}

/// Runs `f`, charging its wall time to `cat` when stats are on.
#[inline]
pub fn timed<T>(cat: Cat, f: impl FnOnce() -> T) -> T {
    if !enabled() {
        return f();
    }
    let start = Instant::now();
    let result = f();
    add(cat, start.elapsed().as_nanos() as u64);
    result
}

/// `timed` plus one on the matching count category.
#[inline]
pub fn timed_counted<T>(cat: Cat, count: Cat, f: impl FnOnce() -> T) -> T {
    if !enabled() {
        return f();
    }
    let start = Instant::now();
    let result = f();
    add(cat, start.elapsed().as_nanos() as u64);
    add(count, 1);
    result
}

/// The calling thread's counters, reset to zero.
pub fn take_thread() -> [u64; CATS] {
    ACC.with(|a| a.replace([0; CATS]))
}
