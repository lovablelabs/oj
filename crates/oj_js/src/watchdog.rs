// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! One long-lived watchdog thread per engine, terminating JS execution when
//! an armed deadline passes. Firing happens with the state lock held and
//! disarm takes the same lock, so a disarm's `fired` answer is settled and a
//! late fire can never poison the job that comes after the one it was armed
//! for.

use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use deno_core::v8;

use crate::lock;

/// A dedicated thread rather than a tokio task: spawn paths do not reliably
/// carry a runtime handle, and the watchdog must keep ticking while the
/// engine thread itself is wedged in synchronous JS.
pub(crate) struct Watchdog {
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

struct Shared {
    state: Mutex<State>,
    cv: Condvar,
}

struct State {
    next_id: u64,
    /// Armed deadlines ordered by expiry; the id disambiguates equal instants.
    /// Several calls park concurrently, each with its own deadline.
    armed: BTreeMap<(Instant, u64), Arc<AtomicBool>>,
    shutdown: bool,
}

impl Watchdog {
    pub(crate) fn spawn(handle: v8::IsolateHandle) -> Watchdog {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                next_id: 0,
                armed: BTreeMap::new(),
                shutdown: false,
            }),
            cv: Condvar::new(),
        });
        let thread = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("oj-js-watchdog".into())
                .spawn(move || run(&shared, &handle))
                .expect("spawn watchdog thread")
        };
        Watchdog {
            shared,
            thread: Some(thread),
        }
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        lock(&self.shared.state).shutdown = true;
        self.shared.cv.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(shared: &Shared, handle: &v8::IsolateHandle) {
    let mut state = lock(&shared.state);
    while !state.shutdown {
        // Fire the earliest due deadline or wait for it — firing holds the
        // lock, so a concurrent disarm reads a settled verdict.
        let now = Instant::now();
        state = match state.armed.first_key_value().map(|(&key, _)| key) {
            Some((at, _)) if at <= now => {
                let (_, fired) = state.armed.pop_first().unwrap();
                fired.store(true, Ordering::SeqCst);
                handle.terminate_execution();
                state
            }
            Some((at, _)) => {
                shared
                    .cv
                    .wait_timeout(state, at.saturating_duration_since(now))
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .0
            }
            None => shared
                .cv
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        };
    }
}

/// One job's handle on the engine's [`Watchdog`]: armed at submit, disarmed
/// when the job settles.
pub(crate) struct DeadlineGuard {
    key: (Instant, u64),
    fired: Arc<AtomicBool>,
    shared: Arc<Shared>,
}

impl DeadlineGuard {
    pub(crate) fn arm(watchdog: &Watchdog, deadline: Duration) -> Self {
        let fired = Arc::new(AtomicBool::new(false));
        let key = {
            let mut state = lock(&watchdog.shared.state);
            let key = (Instant::now() + deadline, state.next_id);
            state.next_id += 1;
            state.armed.insert(key, fired.clone());
            key
        };
        // A new earliest deadline: wake the thread to re-derive its wait.
        watchdog.shared.cv.notify_all();
        DeadlineGuard {
            key,
            fired,
            shared: Arc::clone(&watchdog.shared),
        }
    }

    /// Disarms the watchdog for this job and reports whether it fired.
    /// Taking the lock waits out a fire in progress, so the answer is settled.
    pub(crate) fn disarm(self) -> bool {
        lock(&self.shared.state).armed.remove(&self.key);
        self.fired.load(Ordering::SeqCst)
    }

    /// Whether the watchdog fired, without disarming it.
    pub(crate) fn fired(&self) -> bool {
        let _settled = lock(&self.shared.state);
        self.fired.load(Ordering::SeqCst)
    }
}
