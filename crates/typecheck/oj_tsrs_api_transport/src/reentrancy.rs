// Re-entrancy policy for API handlers (see README "Re-entrancy" and INTEGRATION.md).
//
// A client filesystem callback may issue API requests while the server request that triggered the
// callback is still running. In sync (MessagePack) mode the nested request runs on the very thread that
// is blocked in `Caller::call`; in async (JSON-RPC) mode it runs on another thread. Either way, if the
// nested request blocks on a non-reentrant resource (session mutex, exclusive checker lease) held by a
// request that is waiting for the client, nothing can make progress: the pinned Go server hangs there.
// tsrs instead reports a deliberate error. Handlers do not need the transport's types for this: the
// connection publishes the current request context in a thread-local for the duration of a request.
//
// Ownership. Every request has a `RequestState`. A client call made on a thread that is serving a
// request is *attributed* to that request; a call made on any other thread (compiler worker threads
// reading files during a program build, which carry no request identity) is *unattributed*: the
// protocol does not say which request a callback belongs to, so attribution is only as good as the
// thread that makes the call. A waiter that knows the resource holder (`Holder`, recorded by
// `lock_for_request` and by `ContentionWait` users) treats the holder as possibly stuck on the client
// when the holder (or a request the holder itself waits for, transitively) has an attributed call in
// flight, or when any unattributed call is in flight (it might be the holder's). Otherwise the waiter
// waits like Go, however long the holder takes.
//
// When a possibly-stuck holder is found:
// - Sync: only one top-level request runs at a time, so the waiter is a nested request issued from a
//   callback: the conflict is certain and is reported immediately.
// - Async: the transport cannot know whether the pending callback issued this waiter, so it fails only
//   after the holder has been possibly-stuck for the whole grace period (`ConnOptions::
//   reentrancy_grace`, default 10 s), measured per acquisition. A real async re-entry deadlock ends with
//   the error after the grace period instead of hanging.
//
// Known divergence from pinned Go (async only, bounded): an unrelated waiter is rejected after the
// grace period if the holder itself waits on a client callback for longer than the grace period, or if
// an unattributed callback (any worker-thread file read on the connection) stays pending that long,
// because those cannot be told apart from a re-entry deadlock. Go waits forever in both cases.

use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

use rustc_hash::FxHashMap;

use crate::handler::RequestContext;
use crate::message::ApiError;

pub(crate) const DEFAULT_ASYNC_GRACE: Duration = Duration::from_secs(10);

/// Per-request re-entrancy state: attributed client calls in flight and the request it is blocked on.
#[derive(Debug, Default)]
pub struct RequestState {
    waiting: AtomicUsize,
    blocked_on: Mutex<Option<Arc<RequestState>>>,
}

/// Connection-wide client-call accounting.
#[derive(Debug)]
pub struct CallbackState {
    waiting: AtomicUsize,
    unattributed: AtomicUsize,
    lock: Mutex<()>,
    changed: Condvar,
    sync: bool,
    grace: Duration,
}

impl Default for CallbackState {
    fn default() -> Self {
        CallbackState::new(false, DEFAULT_ASYNC_GRACE)
    }
}

impl CallbackState {
    pub(crate) fn new(sync: bool, grace: Duration) -> CallbackState {
        CallbackState {
            waiting: AtomicUsize::new(0),
            unattributed: AtomicUsize::new(0),
            lock: Mutex::new(()),
            changed: Condvar::new(),
            sync,
            grace,
        }
    }

    /// Client calls in flight on the connection (attributed or not).
    fn waiting_on_client(&self) -> usize {
        self.waiting.load(Ordering::SeqCst)
    }

    /// Client calls in flight that were made outside any request thread.
    fn unattributed_waiting(&self) -> usize {
        self.unattributed.load(Ordering::SeqCst)
    }

    /// Registers a client call made on the current thread until the guard drops.
    pub(crate) fn enter(self: &Arc<Self>) -> WaitingGuard {
        let owner = current_request().map(|cx| cx.state);
        match &owner {
            Some(state) => state.waiting.fetch_add(1, Ordering::SeqCst),
            None => self.unattributed.fetch_add(1, Ordering::SeqCst),
        };
        self.waiting.fetch_add(1, Ordering::SeqCst);
        self.notify();
        WaitingGuard { conn: Arc::clone(self), owner }
    }

    fn notify(&self) {
        let _g = self.lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        self.changed.notify_all();
    }

    fn wait_for_change(&self, timeout: Duration) {
        let g = self.lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = self.changed.wait_timeout(g, timeout);
    }
}

pub(crate) struct WaitingGuard {
    conn: Arc<CallbackState>,
    owner: Option<Arc<RequestState>>,
}

impl Drop for WaitingGuard {
    fn drop(&mut self) {
        match &self.owner {
            Some(state) => state.waiting.fetch_sub(1, Ordering::SeqCst),
            None => self.conn.unattributed.fetch_sub(1, Ordering::SeqCst),
        };
        self.conn.waiting.fetch_sub(1, Ordering::SeqCst);
        self.conn.notify();
    }
}

thread_local! {
    static CURRENT: std::cell::RefCell<Option<RequestContext>> = const { std::cell::RefCell::new(None) };
}

/// Sets the current request context for the duration of `f` (restores the previous one after, so
/// nested sync requests see their own depth).
pub(crate) fn with_request<R>(cx: &RequestContext, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<RequestContext>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let prev = self.0.take();
            CURRENT.with(|c| *c.borrow_mut() = prev);
        }
    }
    let prev = CURRENT.with(|c| c.borrow_mut().replace(cx.clone()));
    let _restore = Restore(prev);
    f()
}

/// The request being handled on this thread, if any. `depth >= 1` means a sync nested request issued
/// from inside a client callback.
pub fn current_request() -> Option<RequestContext> {
    CURRENT.with(|c| c.borrow().clone())
}

/// The current request's identity, to carry onto threads that do work for it (e.g.
///
/// compiler workers reading files through the callback filesystem). Client calls made inside `enter` are
/// attributed to that request instead of being unattributed (see module docs).
#[derive(Clone, Debug)]
pub struct RequestScope(RequestContext);

impl RequestScope {
    /// The request served on this thread (None outside a request).
    pub fn current() -> Option<RequestScope> {
        current_request().map(RequestScope)
    }

    /// Runs `f` on this thread as part of the captured request.
    pub fn enter<R>(&self, f: impl FnOnce() -> R) -> R {
        with_request(&self.0, f)
    }
}

/// The request that holds an exclusive resource. Capture with `Holder::current()` right after
/// acquiring the resource and keep it with the resource until release.
#[derive(Clone, Debug)]
pub struct Holder(Arc<RequestState>);

impl Holder {
    /// The request served on this thread (None outside a request).
    pub fn current() -> Option<Holder> {
        current_request().map(|cx| Holder(cx.state))
    }
}

/// Whether `holder` might be blocked on the client: it (or a request it waits for) has an attributed
/// client call in flight, or an unattributed call is in flight. A wait-for cycle counts as stuck.
fn holder_possibly_stuck(holder: &Arc<RequestState>, conn: &CallbackState) -> bool {
    if conn.unattributed_waiting() > 0 {
        return true;
    }
    let mut r = Arc::clone(holder);
    for _ in 0..64 {
        if r.waiting.load(Ordering::SeqCst) > 0 {
            return true;
        }
        let next = r.blocked_on.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        match next {
            Some(n) if Arc::ptr_eq(&n, holder) => return true,
            Some(n) => r = n,
            None => return false,
        }
    }
    true
}

/// Per-acquisition contention state for an exclusive resource that cannot use `lock_for_request` (e.g.
///
/// a checker lease). Create one when an acquisition first finds the resource taken, call `may_deadlock` on
/// every re-check, and drop it when the acquisition ends (success or failure).
pub struct ContentionWait {
    holder: Option<Arc<RequestState>>,
    stuck_since: Option<Instant>,
    registered: Option<Arc<RequestState>>,
}

impl ContentionWait {
    /// `holder`: the resource's current holder if known (`Holder::current()` captured by the holder).
    /// Without it, any client call in flight on the connection counts as the holder's (conservative).
    pub fn new(holder: Option<&Holder>) -> ContentionWait {
        ContentionWait {
            holder: holder.map(|h| Arc::clone(&h.0)),
            stuck_since: None,
            registered: None,
        }
    }

    /// Updates the holder (it can change between re-checks).
    pub fn set_holder(&mut self, holder: Option<&Holder>) {
        let h = holder.map(|h| Arc::clone(&h.0));
        let same = match (&h, &self.holder) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if !same {
            self.holder = h;
            self.stuck_since = None;
            if let Some(me) = &self.registered {
                (*me.blocked_on.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
                    .clone_from(&self.holder);
            }
        }
    }

    /// True when waiting any longer could deadlock on a client callback: immediately on a sync
    /// connection when the holder is possibly stuck; on an async connection once the holder has been
    /// possibly stuck for the grace period during *this* acquisition. Always false outside a request.
    pub fn may_deadlock(&mut self) -> bool {
        let Some(cx) = current_request() else {
            return false;
        };
        if self.registered.is_none() {
            (*cx.state.blocked_on.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
                .clone_from(&self.holder);
            self.registered = Some(Arc::clone(&cx.state));
        }
        let stuck = match &self.holder {
            Some(h) => holder_possibly_stuck(h, &cx.callbacks),
            None => cx.callbacks.waiting_on_client() > 0,
        };
        if !stuck {
            self.stuck_since = None;
            return false;
        }
        if cx.callbacks.sync {
            return true;
        }
        let now = Instant::now();
        let since = *self.stuck_since.get_or_insert(now);
        now.duration_since(since) >= cx.callbacks.grace
    }
}

impl Drop for ContentionWait {
    fn drop(&mut self) {
        if let Some(me) = &self.registered {
            *me.blocked_on.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        }
    }
}

thread_local! {
    /// (request, start, last check) of the current contended wait on this thread, for the legacy
    /// `blocking_may_deadlock` (no per-acquisition state).
    static EPISODE: Cell<Option<(usize, Instant, Instant)>> = const { Cell::new(None) };
}

const EPISODE_GAP: Duration = Duration::from_millis(200);

/// Legacy predicate without per-acquisition state; prefer `ContentionWait`.
///
/// Treats any client call on the connection as the holder's. On async connections the grace period is tracked
/// per thread and request: a new acquisition by the same request within 200 ms of a previous contended check
/// inherits that wait's start, so it can fail up to one grace period early. Kept so existing callers compile.
pub fn blocking_may_deadlock() -> bool {
    let Some(cx) = current_request() else {
        return false;
    };
    if cx.callbacks.waiting_on_client() == 0 {
        EPISODE.with(|e| e.set(None));
        return false;
    }
    if cx.callbacks.sync {
        return true;
    }
    let key = Arc::as_ptr(&cx.state) as usize;
    let now = Instant::now();
    let start = match EPISODE.with(std::cell::Cell::get) {
        Some((k, start, last)) if k == key && now.duration_since(last) < EPISODE_GAP => start,
        _ => now,
    };
    EPISODE.with(|e| e.set(Some((key, start, now))));
    now.duration_since(start) >= cx.callbacks.grace
}

pub fn reentrancy_error(resource: &str) -> ApiError {
    ApiError::internal(format!(
        "api: client error: {resource} is in use by a request that is waiting on a client callback; \
         API requests made from inside a callback cannot use it until that callback returns"
    ))
}

/// Holders of mutexes locked through `lock_for_request`, keyed by mutex address (entries exist only
/// while the guard is alive).
static LOCK_HOLDERS: Mutex<Option<FxHashMap<usize, Arc<RequestState>>>> = Mutex::new(None);

fn lock_holders() -> MutexGuard<'static, Option<FxHashMap<usize, Arc<RequestState>>>> {
    LOCK_HOLDERS.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Guard returned by `lock_for_request`; derefs to the locked value.
pub struct RequestGuard<'a, T> {
    key: Option<usize>,
    guard: MutexGuard<'a, T>,
}

impl<T> std::ops::Deref for RequestGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> std::ops::DerefMut for RequestGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

impl<T> Drop for RequestGuard<'_, T> {
    fn drop(&mut self) {
        if let Some(key) = self.key
            && let Some(map) = lock_holders().as_mut()
        {
            map.remove(&key);
        }
    }
}

/// Detects callback re-entry that would otherwise deadlock an API request.
///
/// Locks `mutex` for an API request without deadlocking on callback re-entry: waits like Go while the holder
/// makes progress, and fails with `reentrancy_error` when the holder is possibly stuck on the client
/// (immediately on sync, after the grace period on async; see module docs).
///
/// Outside a request it is a plain blocking lock. Poisoned locks are recovered (the connection already
/// reported the panic).
pub fn lock_for_request<'a, T>(
    mutex: &'a Mutex<T>,
    resource: &str,
) -> Result<RequestGuard<'a, T>, ApiError> {
    let key = std::ptr::from_ref::<Mutex<T>>(mutex) as usize;
    let Some(cx) = current_request() else {
        return Ok(RequestGuard {
            key: None,
            guard: mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner),
        });
    };
    let acquired = |guard: MutexGuard<'a, T>| {
        lock_holders().get_or_insert_with(FxHashMap::default).insert(key, Arc::clone(&cx.state));
        RequestGuard { key: Some(key), guard }
    };
    let mut wait: Option<ContentionWait> = None;
    loop {
        match mutex.try_lock() {
            Ok(g) => return Ok(acquired(g)),
            Err(TryLockError::Poisoned(p)) => return Ok(acquired(p.into_inner())),
            Err(TryLockError::WouldBlock) => {}
        }
        let holder = lock_holders().as_ref().and_then(|m| m.get(&key).cloned()).map(Holder);
        let w = wait.get_or_insert_with(|| ContentionWait::new(holder.as_ref()));
        w.set_holder(holder.as_ref());
        if w.may_deadlock() {
            return Err(reentrancy_error(resource));
        }
        if cx.cancel.is_cancelled() {
            return Err(ApiError::internal("ipc: connection closed"));
        }
        // Woken early when a client call starts or ends; otherwise re-check shortly.
        cx.callbacks.wait_for_change(Duration::from_millis(2));
    }
}
