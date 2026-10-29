// Re-entrancy guard for the program's persistent API checker (Go `CheckerLifetimeAPI`).
//
// The API checker is a single exclusive slot per program (`checkerPool.persistentSem`). Pinned Go blocks
// on it unconditionally; if a request holding it is waiting for a client callback (sync MessagePack: on
// the same thread; async: another request) and the client issues a nested request that needs the same
// checker, nothing can make progress. Before taking the slot, the checker lane takes this per-program
// gate instead, using the transport's holder-aware per-acquisition contention state
// (`tsrs_api_transport::{Holder, ContentionWait}`, see the transport's INTEGRATION.md):
// - the acquiring request records `Holder::current()` with the gate and clears it on release;
// - an uncontended acquisition proceeds immediately;
// - a contended acquisition uses one `ContentionWait` for its whole wait, updated whenever the holder
//   changes, and waits like Go while the holder makes progress; it fails with the transport's bounded
//   re-entrancy error only when `may_deadlock()` says the holder is possibly stuck on the client
//   (immediately on sync connections, after the connection's grace period on async ones, measured for
//   this acquisition only).
// Outside a transport request (tests, in-process callers) it is a plain blocking lock.
//
// The gate map is keyed by the program's address. Entries are only created and used while the
// requesting snapshot keeps that program alive, and an entry is removed as soon as no lease or waiter
// refers to it, so a key never outlives its program while in use. Keys never leave this module.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use rustc_hash::FxHashMap;
use tsrs_api_transport::{ContentionWait, Holder};
use tsrs_compiler::Program;

use super::host::{CheckerError, CheckerErrorKind, CheckerResult};

#[derive(Default)]
struct GateState {
    held: bool,
    /// The request holding the gate (None when held outside a transport request).
    holder: Option<Holder>,
    /// Acquisitions waiting on `released` (counted under this lock, so a release with none skips the wake).
    waiters: usize,
}

#[derive(Default)]
struct Gate {
    state: Mutex<GateState>,
    released: Condvar,
}

static GATES: Mutex<Option<FxHashMap<usize, Arc<Gate>>>> = Mutex::new(None);

fn lock_gates() -> std::sync::MutexGuard<'static, Option<FxHashMap<usize, Arc<Gate>>>> {
    GATES.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Exclusive right to acquire the program's API checker; released on drop.
pub struct ApiCheckerLease {
    key: usize,
    gate: Arc<Gate>,
}

const RESOURCE: &str = "the program's API checker";

pub fn acquire(program: &'static Program) -> CheckerResult<ApiCheckerLease> {
    let key = std::ptr::from_ref::<Program>(program) as usize;
    let gate =
        Arc::clone(lock_gates().get_or_insert_with(FxHashMap::default).entry(key).or_default());
    let lease = |gate: Arc<Gate>| ApiCheckerLease { key, gate };
    let result = (|| {
        let mut st = gate.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        // One contention state per acquisition (created on first contention, dropped when this
        // acquisition ends, which also clears this request's wait-for edge).
        let mut wait: Option<ContentionWait> = None;
        loop {
            if !st.held {
                st.held = true;
                st.holder = Holder::current();
                return Ok(());
            }
            let Some(cx) = tsrs_api_transport::current_request() else {
                st.waiters += 1;
                st = gate.released.wait(st).unwrap_or_else(std::sync::PoisonError::into_inner);
                st.waiters -= 1;
                continue;
            };
            let w = wait.get_or_insert_with(|| ContentionWait::new(st.holder.as_ref()));
            w.set_holder(st.holder.as_ref());
            if w.may_deadlock() {
                return Err(CheckerError {
                    kind: CheckerErrorKind::Internal,
                    message: tsrs_api_transport::reentrancy::reentrancy_error(RESOURCE).message,
                });
            }
            if cx.cancel.is_cancelled() {
                return Err(CheckerError {
                    kind: CheckerErrorKind::Internal,
                    message: "ipc: connection closed".to_string(),
                });
            }
            // Re-check shortly: the holder may start or stop waiting on the client while we wait.
            st.waiters += 1;
            st = gate
                .released
                .wait_timeout(st, Duration::from_millis(2))
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
            st.waiters -= 1;
        }
    })();
    match result {
        Ok(()) => Ok(lease(gate)),
        Err(e) => {
            forget_if_unused(key, &gate);
            Err(e)
        }
    }
}

fn forget_if_unused(key: usize, gate: &Arc<Gate>) {
    let mut gates = lock_gates();
    // One reference in the map plus the caller's: nobody else holds or waits on this gate.
    if Arc::strong_count(gate) == 2
        && !gate.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).held
        && let Some(map) = gates.as_mut()
    {
        map.remove(&key);
    }
}

impl Drop for ApiCheckerLease {
    fn drop(&mut self) {
        let waiters = {
            let mut st = self.gate.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            st.held = false;
            st.holder = None;
            st.waiters
        };
        // A wake is a system call even when nobody waits.
        if waiters > 0 {
            self.gate.released.notify_all();
        }
        forget_if_unused(self.key, &self.gate);
    }
}

pub fn is_tracked(program: &'static Program) -> bool {
    lock_gates()
        .as_ref()
        .is_some_and(|m| m.contains_key(&(std::ptr::from_ref::<Program>(program) as usize)))
}
