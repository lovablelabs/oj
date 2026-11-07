use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const DEFAULT_MIN_WAIT_TIME: Duration = Duration::from_millis(50);
pub const DEFAULT_MAX_WAIT_TIME: Duration = Duration::from_millis(500);

// Go keeps these as package variables so tests can shorten them; nothing changes them here.
const MIN_WAIT_TIME: Duration = DEFAULT_MIN_WAIT_TIME;
const MAX_WAIT_TIME: Duration = DEFAULT_MAX_WAIT_TIME;

type DebounceCallback = Arc<dyn Fn() + Send + Sync>;

// debounce batches filesystem events for one backend. Each *watcher
// owns one debounce instance, created lazily on first subscribe and
// living for the process lifetime. The background goroutine costs
// nothing when idle.
//
// Per-backend (rather than process-wide) isolation means a slow user
// callback on one backend cannot starve event delivery on the others.
//
// Internally uses a resettable latch: the loop blocks until trigger()
// is called, then coalesces for minWaitTime before firing callbacks.
//
// Go's latch channels (`waitCh` closed = signalled, `triggerCh` replaced on each trigger) are a mutex-guarded
// `notified` flag and trigger generation with a condition variable.
pub struct Debounce {
    mu: Mutex<DebounceState>,
    latch_mu: Mutex<LatchState>,
    latch_cv: Condvar,
    // tsrs-only (`set_write_settling`): files a writer has written or created and not closed yet, with the times of
    // the first and the last write.
    writes: Mutex<WriteState>,
}

#[derive(Default)]
struct WriteState {
    open: FxHashMap<String, (Instant, Instant)>,
    // Files that stopped holding the callbacks (`settleIdleTime`, `settleMaxTime`) and are still open: a writer that
    // keeps its file open (an editor's swap file, a log) would otherwise hold every cycle of the session for up to
    // `settleMaxTime`. They hold nothing again until they are closed.
    long_lived: FxHashSet<String>,
}

// tsrs-only (`set_write_settling`): a file that was written to but not closed stops holding the callbacks once it has
// not been written for `settleIdleTime`, or `settleMaxTime` after its first write (a writer that keeps the file open,
// such as a log).
const SETTLE_IDLE_TIME: Duration = Duration::from_millis(1000);
const SETTLE_MAX_TIME: Duration = Duration::from_millis(2000);

struct DebounceState {
    callbacks: FxHashMap<usize, DebounceCallback>,
    last_time: Option<Instant>,
}

#[derive(Default)]
struct LatchState {
    notified: bool,
    trigger_gen: u64,
}

impl Debounce {
    // debounce.go:42
    pub fn new() -> Arc<Debounce> {
        let d = Arc::new(Debounce {
            mu: Mutex::new(DebounceState { callbacks: FxHashMap::default(), last_time: None }),
            latch_mu: Mutex::new(LatchState::default()),
            latch_cv: Condvar::new(),
            writes: Mutex::new(WriteState::default()),
        });
        let looper = Arc::clone(&d);
        std::thread::Builder::new()
            .name("fswatch-debounce".to_string())
            .spawn(move || looper.loop_())
            .expect("failed to spawn fswatch debounce thread");
        d
    }

    // debounce.go:51
    // add registers a callback under key.
    pub(crate) fn add(&self, key: usize, cb: DebounceCallback) {
        self.mu.lock().unwrap().callbacks.insert(key, cb);
    }

    // debounce.go:58
    // remove deregisters the callback for key.
    pub(crate) fn remove(&self, key: usize) {
        self.mu.lock().unwrap().callbacks.remove(&key);
    }

    // debounce.go:65
    // trigger wakes the debounce loop.
    #[cfg(unix)]
    pub(crate) fn trigger(&self) {
        let mut l = self.latch_mu.lock().unwrap();
        if !l.notified {
            l.notified = true;
        }
        l.trigger_gen += 1;
        self.latch_cv.notify_all();
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    // tsrs-only: a writer wrote to `path` (or created it) and has not closed it yet.
    pub(crate) fn write_started(&self, path: &str) {
        let now = Instant::now();
        let mut writes = self.writes.lock().unwrap();
        if writes.long_lived.contains(path) {
            return;
        }
        match writes.open.get_mut(path) {
            Some(times) => times.1 = now,
            None => {
                writes.open.insert(path.to_string(), (now, now));
            }
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    // tsrs-only: the writer of `path` closed it; the callbacks fire once no file is being written.
    pub(crate) fn write_finished(&self, path: &str) {
        let mut writes = self.writes.lock().unwrap();
        writes.long_lived.remove(path);
        if writes.open.remove(path).is_some() && writes.open.is_empty() {
            drop(writes);
            self.trigger();
        }
    }

    #[cfg(target_os = "macos")]
    // tsrs-only: the events about to be triggered start a burst of writes. Unless the callbacks fired recently, this
    // counts as a fire, so that they wait for a quiet period (`minWaitTime`, at most `maxWaitTime`) instead of firing
    // at the first event.
    pub(crate) fn hold_leading_edge(&self) {
        let mut st = self.mu.lock().unwrap();
        let now = Instant::now();
        if st.last_time.is_none_or(|last| now.duration_since(last) > MAX_WAIT_TIME) {
            st.last_time = Some(now);
        }
    }

    // When a file is still being written: the time at which the first of them stops holding the callbacks.
    fn pending_write_deadline(&self) -> Option<Instant> {
        let now = Instant::now();
        let mut writes = self.writes.lock().unwrap();
        let WriteState { open, long_lived } = &mut *writes;
        open.retain(|path, &mut (first, last)| {
            let settling = now.duration_since(last) < SETTLE_IDLE_TIME
                && now.duration_since(first) < SETTLE_MAX_TIME;
            if !settling {
                long_lived.insert(path.clone());
            }
            settling
        });
        open.values()
            .map(|&(first, last)| (last + SETTLE_IDLE_TIME).min(first + SETTLE_MAX_TIME))
            .min()
    }

    // debounce.go:76
    fn loop_(&self) -> ! {
        loop {
            self.latch_wait();
            self.notify_if_ready();
        }
    }

    // debounce.go:83
    fn notify_if_ready(&self) {
        // tsrs-only: while a file is being written, its events would make the callbacks read it half-written (Go fires
        // the first event after a quiet period at once, which for a save is the truncation). Wait for the close, or
        // for a new event, then decide again (the latch stays notified).
        if let Some(deadline) = self.pending_write_deadline() {
            let l = self.latch_mu.lock().unwrap();
            let r#gen = l.trigger_gen;
            let wait = deadline.saturating_duration_since(Instant::now());
            drop(self.latch_cv.wait_timeout_while(l, wait, |l| l.trigger_gen == r#gen).unwrap());
            return;
        }
        let mut st = self.mu.lock().unwrap();
        let now = Instant::now();
        // Go's zero lastTime makes the first gap larger than maxWaitTime.
        let gap_exceeded = st.last_time.is_none_or(|last| now.duration_since(last) > MAX_WAIT_TIME);
        if gap_exceeded {
            st.last_time = Some(now);
            drop(st);
            self.fire_callbacks();
            return;
        }
        drop(st);
        self.coalesce_wait();
    }

    // debounce.go:97
    fn coalesce_wait(&self) {
        let l = self.latch_mu.lock().unwrap();
        let r#gen = l.trigger_gen;
        let (l, timeout) =
            self.latch_cv.wait_timeout_while(l, MIN_WAIT_TIME, |l| l.trigger_gen == r#gen).unwrap();
        drop(l);
        if timeout.timed_out() {
            self.fire_callbacks();
        }
        // Otherwise a new event triggered; fire on the next tick.
    }

    // debounce.go:110
    // fireCallbacks snapshots and invokes all registered callbacks.
    fn fire_callbacks(&self) {
        let mut st = self.mu.lock().unwrap();
        st.last_time = Some(Instant::now());
        let cbs: Vec<DebounceCallback> = st.callbacks.values().cloned().collect();
        drop(st);

        self.latch_reset();

        for cb in cbs {
            cb();
        }
    }

    // debounce.go:140
    fn latch_wait(&self) {
        let l = self.latch_mu.lock().unwrap();
        let _l = self.latch_cv.wait_while(l, |l| !l.notified).unwrap();
    }

    // debounce.go:147
    fn latch_reset(&self) {
        let mut l = self.latch_mu.lock().unwrap();
        if l.notified {
            l.notified = false;
        }
    }
}

impl Debounce {
    // Drive the real event classifier synchronously without a background thread draining its pending batch.
    pub fn new_without_loop_for_test() -> Arc<Self> {
        Arc::new(Self {
            mu: Mutex::new(DebounceState { callbacks: FxHashMap::default(), last_time: None }),
            latch_mu: Mutex::new(LatchState::default()),
            latch_cv: Condvar::new(),
            writes: Mutex::new(WriteState::default()),
        })
    }

    pub fn leading_edge_is_held_for_test(&self) -> bool {
        self.mu.lock().unwrap().last_time.is_some()
    }
}
