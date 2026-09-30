// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! One long-lived watchdog thread per engine, terminating JS execution when
//! an armed deadline passes. Firing removes the deadline from the armed set
//! under the state lock, so "did it fire" IS "is my key gone" — no flag to
//! keep in sync — and a disarm taking the same lock reads a settled answer;
//! a late fire can never poison the job that comes after the one it was
//! armed for.

use std::collections::BTreeSet;
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
    armed: BTreeSet<(Instant, u64)>,
    shutdown: bool,
}

impl Watchdog {
    pub(crate) fn spawn(handle: v8::IsolateHandle) -> Watchdog {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                next_id: 0,
                armed: BTreeSet::new(),
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
        // Fire the earliest due deadline or wait for it.
        let now = Instant::now();
        state = match state.armed.first().copied() {
            Some((at, _)) if at <= now => {
                state.armed.pop_first();
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
    shared: Arc<Shared>,
}

impl DeadlineGuard {
    pub(crate) fn arm(watchdog: &Watchdog, deadline: Duration) -> Self {
        let key = {
            let mut state = lock(&watchdog.shared.state);
            let key = (Instant::now() + deadline, state.next_id);
            state.next_id += 1;
            state.armed.insert(key);
            key
        };
        // A new earliest deadline: wake the thread to re-derive its wait.
        watchdog.shared.cv.notify_all();
        DeadlineGuard {
            key,
            shared: Arc::clone(&watchdog.shared),
        }
    }

    /// Disarms the watchdog for this job and reports whether it fired: the
    /// key already gone means the watchdog removed it when it terminated.
    pub(crate) fn disarm(self) -> bool {
        !lock(&self.shared.state).armed.remove(&self.key)
    }

    /// Whether the watchdog fired, without disarming it.
    pub(crate) fn fired(&self) -> bool {
        !lock(&self.shared.state).armed.contains(&self.key)
    }
}
