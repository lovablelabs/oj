// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Returns freed heap to the OS once a burst of requests has gone quiet.
//!
//! A page load compiles its modules on many threads at once and frees the
//! work when it is served. glibc keeps those pages resident in each thread's
//! arena until `malloc_trim` is called, so the process holds its busiest
//! moment for the rest of the session.

/// How long the server must serve nothing before the heap is trimmed, and
/// how often that is checked.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
const QUIET: std::time::Duration = std::time::Duration::from_secs(5);

/// `router` with one [`crate::rolldown_runtime::release_freed_heap`] after
/// each burst of requests. As it is off glibc, where there is nothing to trim.
pub(crate) fn trim_when_quiet(router: axum::Router) -> axum::Router {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::Arc;

        // Counts a request when it arrives and when it is answered, so one
        // that outlasts QUIET still ends in a trim.
        let events = Arc::new(AtomicU64::new(0));
        let seen = Arc::clone(&events);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(QUIET);
            let (mut last, mut owed) = (0, false);
            loop {
                tick.tick().await;
                let now = seen.load(Ordering::Relaxed);
                if now != last {
                    last = now;
                    owed = true;
                } else if owed {
                    owed = false;
                    let _ =
                        tokio::task::spawn_blocking(crate::rolldown_runtime::release_freed_heap)
                            .await;
                }
            }
        });
        router.layer(axum::middleware::from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                let events = Arc::clone(&events);
                async move {
                    events.fetch_add(1, Ordering::Relaxed);
                    let response = next.run(req).await;
                    events.fetch_add(1, Ordering::Relaxed);
                    response
                }
            },
        ))
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    router
}
