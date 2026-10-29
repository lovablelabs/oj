//! A limit on how many threads run language-server request work at once: Go's `GOMAXPROCS` for the goroutines a
//! request starts.
//!
//! The language server starts a thread per request in flight (`tsrs_lsp` `workerpool`), so a request never waits for
//! one that is blocked, as with Go's goroutine per request. Go runs at most `GOMAXPROCS` goroutines at once, though,
//! and a burst of requests here made as many threads runnable as there were requests. Under a CPU quota smaller than
//! the machine (a container with 8 CPUs of quota on a 64-CPU host), the kernel spread them over many CPUs and spent
//! more time than the requests did (40 concurrent folding range requests: 9 s of system time in 1.2 s, 4x the time
//! of running them one at a time).
//!
//! A job takes a slot to run (`run`) and gives it back while it waits for something another job may hold
//! (`blocking`: a checker, a client response, another thread forcing a lazy list), as a blocked goroutine gives back
//! its P, so a waiting job never keeps a runnable one from running. A wait that is not marked only keeps its slot.
//! A job coming back from `blocking` takes its slot back without waiting, even over capacity: it may hold a lock
//! that every slot holder is waiting for, and waiting for a slot there would deadlock. New jobs (`run`) still wait
//! until fewer than the capacity run.
//! Threads that never took a slot (the dispatch loop, background tasks) are not limited, and `blocking` is a no-op
//! on them.

use std::cell::Cell;
use std::sync::{Condvar, Mutex, OnceLock};

struct Slots {
    in_use: Mutex<usize>,
    freed: Condvar,
    capacity: usize,
}

thread_local! {
    pub static HOLDS_SLOT: Cell<bool> = const { Cell::new(false) };
}

fn get_slots() -> &'static Slots {
    static SLOTS: OnceLock<Slots> = OnceLock::new();
    SLOTS.get_or_init(|| Slots {
        in_use: Mutex::new(0),
        freed: Condvar::new(),
        capacity: std::thread::available_parallelism().map_or(1, std::num::NonZero::get),
    })
}

/// Runs `f` once a slot is free, holding it while `f` runs (outside of `blocking` calls). A nested `run` keeps the
/// slot it has.
pub fn run<R>(f: impl FnOnce() -> R) -> R {
    if HOLDS_SLOT.with(Cell::get) {
        return f();
    }
    acquire();
    let _release = ReleaseOnDrop;
    f()
}

/// Runs `f`, which waits, without holding this thread's slot, and takes its slot back before returning (without
/// waiting for one to be free).
pub fn blocking<R>(f: impl FnOnce() -> R) -> R {
    if !HOLDS_SLOT.with(Cell::get) {
        return f();
    }
    release();
    let _reacquire = ReacquireOnDrop;
    f()
}

fn acquire() {
    let s = get_slots();
    let mut in_use = s.in_use.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    while *in_use >= s.capacity {
        in_use = s.freed.wait(in_use).unwrap_or_else(std::sync::PoisonError::into_inner);
    }
    *in_use += 1;
    HOLDS_SLOT.with(|h| h.set(true));
}

fn release() {
    HOLDS_SLOT.with(|h| h.set(false));
    let s = get_slots();
    *s.in_use.lock().unwrap_or_else(std::sync::PoisonError::into_inner) -= 1;
    s.freed.notify_one();
}

// Also on unwind: a panicking job (recovered by the server) gives its slot back.
struct ReleaseOnDrop;

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        release();
    }
}

struct ReacquireOnDrop;

impl Drop for ReacquireOnDrop {
    fn drop(&mut self) {
        let s = get_slots();
        *s.in_use.lock().unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
        HOLDS_SLOT.with(|h| h.set(true));
    }
}

pub fn capacity() -> usize {
    get_slots().capacity
}
