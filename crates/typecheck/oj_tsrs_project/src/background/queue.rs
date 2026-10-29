use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, OnceLock, RwLock};

use tsrs_core::context::Context;

// Background work recurses like request work (it may build programs and run checkers).
const WORKER_STACK_SIZE: usize = 512 << 20;
// Debounced tasks (snapshot updates, diagnostics refreshes) sleep on their worker, so keep a few spare.
const WORKER_COUNT: usize = 6;

type Task = Box<dyn FnOnce() + Send>;

// Queue manages background tasks execution
//
// Go starts a goroutine per task (`wg.Go`). Every OS thread owns a leak arena (docs/LSP.md "Threading
// model"), so tasks run on a fixed set of worker threads shared by all queues instead, started on first use;
// `Wait` waits for this queue's tasks like Go's WaitGroup (tasks enqueued by running tasks included).
pub struct Queue {
    pending: Arc<PendingCount>,
    closed: RwLock<bool>,
}

struct PendingCount {
    count: Mutex<usize>,
    zero: Condvar,
}

struct WorkerPool {
    tasks: Mutex<VecDeque<Task>>,
    available: Condvar,
}

#[expect(
    clippy::infinite_loop,
    reason = "background worker threads serve the process-wide queue until process exit"
)]
fn worker_pool() -> &'static WorkerPool {
    static POOL: OnceLock<&'static WorkerPool> = OnceLock::new();
    POOL.get_or_init(|| {
        let pool: &'static WorkerPool = Box::leak(Box::new(WorkerPool {
            tasks: Mutex::new(VecDeque::new()),
            available: Condvar::new(),
        }));
        for i in 0..WORKER_COUNT {
            std::thread::Builder::new()
                .name(format!("project-background-{i}"))
                .stack_size(WORKER_STACK_SIZE)
                .spawn(move || {
                    loop {
                        let task = {
                            let mut tasks = pool.tasks.lock().unwrap();
                            loop {
                                if let Some(task) = tasks.pop_front() {
                                    break task;
                                }
                                tasks = pool.available.wait(tasks).unwrap();
                            }
                        };
                        // Go: a panicking task crashes the process. Here the panic hook reports it and the shared
                        // worker keeps serving other queues.
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(task));
                    }
                })
                .expect("failed to spawn background worker thread");
        }
        pool
    })
}

// Go `go f()` for work that is not tracked by a Queue (timer callbacks): runs `f` on the shared workers.
pub(crate) fn go(f: impl FnOnce() + Send + 'static) {
    let pool = worker_pool();
    pool.tasks.lock().unwrap().push_back(Box::new(f));
    pool.available.notify_one();
}

// queue.go:16
// NewQueue creates a new background queue for managing background tasks execution.
pub fn new_queue() -> Queue {
    Queue {
        pending: Arc::new(PendingCount { count: Mutex::new(0), zero: Condvar::new() }),
        closed: RwLock::new(false),
    }
}

impl Queue {
    // queue.go:20
    pub fn enqueue(&self, ctx: &Context, f: impl FnOnce(&Context) + Send + 'static) {
        let closed = self.closed.read().unwrap();
        if *closed {
            return;
        }

        // Don't start new tasks if context is already cancelled
        if ctx.err().is_some() {
            return;
        }

        *self.pending.count.lock().unwrap() += 1;
        drop(closed);
        let pending = Arc::clone(&self.pending);
        let ctx = ctx.clone();
        let task: Task = Box::new(move || {
            let done = ScopeDone(pending);
            // Check context again before executing
            if ctx.err().is_some() {
                return;
            }
            f(&ctx);
            drop(done);
        });
        let pool = worker_pool();
        pool.tasks.lock().unwrap().push_back(task);
        pool.available.notify_one();
    }

    // queue.go:44
    // Wait waits for all active tasks to complete.
    // It does not prevent new tasks from being enqueued while waiting.
    pub fn wait(&self) {
        // Waits for background workers, without holding a run slot (`tsrs_core::runslots`).
        tsrs_core::runslots::blocking(|| {
            let mut count = self.pending.count.lock().unwrap();
            while *count > 0 {
                count = self.pending.zero.wait(count).unwrap();
            }
        });
    }

    // queue.go:48
    pub fn close(&self) {
        *self.closed.write().unwrap() = true;
        self.wait();
    }
}

// Go's `defer wg.Done()` (also on panic).
struct ScopeDone(Arc<PendingCount>);

impl Drop for ScopeDone {
    fn drop(&mut self) {
        let mut count = self.0.count.lock().unwrap();
        *count -= 1;
        if *count == 0 {
            self.0.zero.notify_all();
        }
    }
}
