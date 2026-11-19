// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Returns freed heap to the OS once a burst of requests has gone quiet.
//!
//! A page load compiles its modules on many threads at once and frees the
//! work when it is served. glibc keeps most of those pages resident in each
//! thread's arena until `malloc_trim` is called, so the process holds its
//! busiest moment for the rest of the session.

/// How often the server is checked for quiet. A trim follows the last
/// request by one to two of these.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
const CHECK_EVERY: std::time::Duration = std::time::Duration::from_secs(5);

/// When a trim is due, from the request counters read at each check.
#[cfg(any(test, all(target_os = "linux", target_env = "gnu")))]
#[derive(Default)]
struct Quiet {
    /// The count of request starts and ends at the last check.
    last: u64,
    /// Whether a request was served since the last trim.
    owed: bool,
}

#[cfg(any(test, all(target_os = "linux", target_env = "gnu")))]
impl Quiet {
    /// Whether to trim now: a request was served since the last trim, none
    /// started or ended since the last check, and none is in flight.
    fn check(&mut self, events: u64, in_flight: usize) -> bool {
        if events != self.last {
            self.last = events;
            self.owed = true;
            return false;
        }
        let trim = self.owed && in_flight == 0;
        self.owed &= !trim;
        trim
    }
}

/// `router` with one [`crate::rolldown_runtime::release_freed_heap`] after
/// each burst of requests. Returns `router` as it is off glibc.
pub(crate) fn trim_when_quiet(router: axum::Router) -> axum::Router {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
        use std::sync::Arc;

        #[derive(Default)]
        struct Served {
            events: AtomicU64,
            in_flight: AtomicUsize,
        }
        /// One request in flight; a dropped request (the client left) ends too.
        struct InFlight(Arc<Served>);
        impl Drop for InFlight {
            fn drop(&mut self) {
                self.0.in_flight.fetch_sub(1, Ordering::Relaxed);
                self.0.events.fetch_add(1, Ordering::Relaxed);
            }
        }

        let served = Arc::new(Served::default());
        // Ends with the router: the task holds no count of its own.
        let seen = Arc::downgrade(&served);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(CHECK_EVERY);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut quiet = Quiet::default();
            loop {
                tick.tick().await;
                let Some(served) = seen.upgrade() else { break };
                let due = quiet.check(
                    served.events.load(Ordering::Relaxed),
                    served.in_flight.load(Ordering::Relaxed),
                );
                drop(served);
                if due {
                    let _ =
                        tokio::task::spawn_blocking(crate::rolldown_runtime::release_freed_heap)
                            .await;
                }
            }
        });
        router.layer(axum::middleware::from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                served.in_flight.fetch_add(1, Ordering::Relaxed);
                served.events.fetch_add(1, Ordering::Relaxed);
                let in_flight = InFlight(Arc::clone(&served));
                async move {
                    let response = next.run(req).await;
                    drop(in_flight);
                    response
                }
            },
        ))
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    router
}

#[cfg(test)]
#[path = "quiet_trim_tests.rs"]
mod tests;
