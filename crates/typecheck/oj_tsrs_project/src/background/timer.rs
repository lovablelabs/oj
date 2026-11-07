// Go `time.AfterFunc` / `(*time.Timer).Reset` / `Stop` (no Go file: docs/LSP.md "Threading model" maps Go's
// timers to one timer thread with a deadline heap). Callbacks run on the background workers (Go: each in its own
// goroutine), so a slow callback does not delay other timers.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use rustc_hash::FxHashMap;

use super::queue::go;

type Callback = Arc<dyn Fn() + Send + Sync>;

struct TimerState {
    heap: BinaryHeap<Reverse<(Instant, u64, u64)>>,
    // timer id -> (generation of its pending deadline, callback); absent when stopped or fired.
    pending: FxHashMap<u64, (u64, Callback)>,
    next_id: u64,
    next_generation: u64,
}

struct TimerThread {
    st: Mutex<TimerState>,
    wake: Condvar,
}

#[expect(
    clippy::infinite_loop,
    reason = "the process-wide timer thread waits for deadlines until process exit"
)]
fn timer_thread() -> &'static TimerThread {
    static TIMERS: OnceLock<&'static TimerThread> = OnceLock::new();
    TIMERS.get_or_init(|| {
        let t: &'static TimerThread = Box::leak(Box::new(TimerThread {
            st: Mutex::new(TimerState {
                heap: BinaryHeap::new(),
                pending: FxHashMap::default(),
                next_id: 0,
                next_generation: 0,
            }),
            wake: Condvar::new(),
        }));
        std::thread::Builder::new()
            .name("project-timers".to_string())
            .spawn(move || {
                let mut st = t.st.lock().unwrap();
                loop {
                    let now = Instant::now();
                    match st.heap.peek().copied() {
                        None => st = t.wake.wait(st).unwrap(),
                        Some(Reverse((deadline, id, generation))) => {
                            if deadline > now {
                                st = t.wake.wait_timeout(st, deadline - now).unwrap().0;
                                continue;
                            }
                            st.heap.pop();
                            let fire = match st.pending.get(&id) {
                                Some((g, f)) if *g == generation => Some(Arc::clone(f)),
                                _ => None,
                            };
                            if let Some(f) = fire {
                                st.pending.remove(&id);
                                go(move || f());
                            }
                        }
                    }
                }
            })
            .expect("failed to spawn timer thread");
        t
    })
}

// Go `*time.Timer` created by `time.AfterFunc`.
pub struct Timer {
    id: u64,
    f: Callback,
}

// Go `time.AfterFunc(d, f)`.
pub fn after_func(d: Duration, f: impl Fn() + Send + Sync + 'static) -> Timer {
    let t = timer_thread();
    let f: Callback = Arc::new(f);
    let mut st = t.st.lock().unwrap();
    st.next_id += 1;
    let id = st.next_id;
    schedule(&mut st, id, d, Arc::clone(&f));
    drop(st);
    t.wake.notify_one();
    Timer { id, f }
}

fn schedule(st: &mut TimerState, id: u64, d: Duration, f: Callback) {
    st.next_generation += 1;
    let generation = st.next_generation;
    st.pending.insert(id, (generation, f));
    st.heap.push(Reverse((Instant::now() + d, id, generation)));
}

impl Timer {
    // Go `t.Stop()`: reports whether the call stopped the timer (false if it already fired or was stopped).
    pub fn stop(&self) -> bool {
        let t = timer_thread();
        t.st.lock().unwrap().pending.remove(&self.id).is_some()
    }

    // Go `t.Reset(d)`: re-arms the timer (also after it fired); reports whether it was pending.
    pub(crate) fn reset(&self, d: Duration) -> bool {
        let t = timer_thread();
        let mut st = t.st.lock().unwrap();
        let was_pending = st.pending.contains_key(&self.id);
        schedule(&mut st, self.id, d, Arc::clone(&self.f));
        drop(st);
        t.wake.notify_one();
        was_pending
    }
}
