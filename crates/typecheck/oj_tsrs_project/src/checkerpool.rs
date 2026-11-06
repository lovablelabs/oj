use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

use rustc_hash::FxHashMap;
use tsrs_ast::{Diagnostic, SourceFile};
use tsrs_compiler::{
    Checker, CheckerHandle, CheckerPool, PooledChecker, Program, sort_and_deduplicate_diagnostics,
};
use tsrs_core::arena::{Region, RegionScope};
use tsrs_core::context::{CheckerLifetime, Context, get_checker_lifetime, get_request_id};

use crate::memregions::{ProgramOwner, log_region};
use tsrs_core::P;

use crate::background::{Timer, after_func};

// checkerHeldAnonymous is a sentinel stored in heldBy when a checker is held
// by a caller that has no request ID (e.g., context.Background()). This
// distinguishes "held without ID" from "not held" (empty string).
const CHECKER_HELD_ANONYMOUS: &str = "<anonymous>";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CheckerPoolOptions {
    // MaxCheckers controls the total number of checker slots per project
    // (1 dedicated diagnostics checker + N-1 query checkers). Minimum 2.
    // Zero uses the default (4).
    pub max_checkers: usize,
    // IdleTimeout controls how long an idle checker is kept
    // before being disposed. Zero uses the default (30s).
    pub idle_timeout: Duration,
}

// Go's buffered channel used as a counting semaphore (`sem <- struct{}{}` / `<-sem`).
struct Semaphore {
    state: Mutex<SemaphoreState>,
    capacity: usize,
    freed: Condvar,
}

#[derive(Default)]
struct SemaphoreState {
    used: usize,
    /// Acquirers waiting on `freed` (counted under the lock, so a release with none skips the wake).
    waiters: usize,
}

impl Semaphore {
    fn new(capacity: usize) -> Semaphore {
        Semaphore { state: Mutex::default(), capacity, freed: Condvar::new() }
    }

    fn acquire(&self) {
        let mut state = self.state.lock().unwrap();
        if state.used >= self.capacity {
            // Waits for another request's checker without holding a run slot (`runslots`).
            drop(state);
            state = tsrs_core::runslots::blocking(|| {
                let mut state = self.state.lock().unwrap();
                while state.used >= self.capacity {
                    state.waiters += 1;
                    state = self.freed.wait(state).unwrap();
                    state.waiters -= 1;
                }
                state
            });
        }
        state.used += 1;
    }

    fn release(&self) {
        let waiters = {
            let mut state = self.state.lock().unwrap();
            state.used -= 1;
            state.waiters
        };
        // A wake is a system call even when nobody waits.
        if waiters > 0 {
            self.freed.notify_one();
        }
    }
}

// checkerPool manages three categories of type checkers for a project:
//
//   - Diagnostics (index 0): A single checker for LSP diagnostics, providing
//     consistent walk order. Idle-cleaned.
//   - Temporary (indices 1+): Ephemeral query checkers for LSP operations.
//     Idle-cleaned after a configurable timeout.
//   - API: A single checker for API operations, providing stable
//     instance identity for reference equality on type/symbol handles.
//     Never idle-cleaned.
pub struct CheckerPoolImpl {
    opts: CheckerPoolOptions,
    program: &'static Program,

    mu: Mutex<CheckerPoolState>,

    diag_sem: Semaphore,
    query_sem: Semaphore,
    persistent_sem: Semaphore,

    log: Box<dyn Fn(&str) + Send + Sync>,
    self_ref: Weak<CheckerPoolImpl>,

    // Memory regions (docs/LSP.md "Memory management"), not in Go. Each checker allocates in its own region (keyed by
    // the checker's address): while it is created and while it is held, the region is the holding thread's
    // allocation target. Go drops a disposed checker and the GC frees it once nothing refers to its data; here a
    // disposed checker and its region are freed once no request that held it can still use its data (its leases,
    // see `checkerDataLease`), unless its data may be referenced from pool- or program-lifetime structures: the
    // checker was held without a lease (`pinned`; the API checker always is) or something stored its data in such a
    // structure (`Region::escaped`: global diagnostics, the program's declaration diagnostics cache, the include
    // processor's diagnostics). Those are parked until the pool is freed (`free_checkers`, called when the program is
    // freed). Declared last: dropped after the checkers in `mu`.
    parked: Mutex<Vec<PooledChecker>>,
    instances: Mutex<FxHashMap<usize, CheckerInstance>>,
    // The program's owner (memregions.rs), once the project system created it. A held checker keeps it alive, and
    // a pool whose program was freed refuses to hand out checkers (Go's GC would keep the program alive through the
    // pool's program pointer; here the project values own it).
    owner: Mutex<Option<Weak<ProgramOwner>>>,
}

struct CheckerPoolState {
    // discarded is set when the pool's program has been replaced. The pool
    // remains fully functional but stops its idle-cleanup timer so that
    // query checkers are not disposed until the pool is GC'd.
    discarded: bool,

    // checkers[0] is the diagnostics checker.
    // checkers[1:] are ephemeral query checkers.
    // All are idle-cleaned.
    checkers: Vec<Option<PooledChecker>>,
    held_by: Vec<String>, // heldBy[i] is the requestID holding checker i, checkerHeldAnonymous, or "" if not held
    file_associations: FxHashMap<P<SourceFile>, usize>, // file → query checker index (1+)
    request_associations: FxHashMap<String, usize>, // requestID → checker index

    // lastReleased tracks when each checker was last released.
    last_released: Vec<Option<Instant>>,

    // cleanupTimer is reset each time a checker is released.
    // When it fires, idle checkers are disposed.
    cleanup_timer: Option<Timer>,

    // persistentChecker is the API checker. It is never idle-cleaned,
    // providing stable instance identity for API clients.
    persistent_checker: Option<PooledChecker>,
    persistent_held: bool,

    global_diag_accumulated: Vec<P<Diagnostic>>,
    global_diag_changed: bool,
    global_diag_checker_count: Vec<usize>, // per-checker count of globals last seen
}

// Region bookkeeping of one checker (see `checkerPool::instances`). Dropping it frees the disposed checker, then the
// region (field order).
struct CheckerInstance {
    // Disposed, waiting for its last lease to end.
    disposed: Option<PooledChecker>,
    region: Region,
    // Live leases of requests that held the checker.
    leases: usize,
    // Held without a lease: whoever held it may use its data for as long as the program lives.
    pinned: bool,
}

// tsrs-only (memory regions): a request's claim on the data of the checkers it held. Go's language service keeps
// using a checker's results after releasing it (diagnostics are converted after `GetSemanticDiagnostics` returned the
// checker), which the GC makes safe; here a disposed checker's region is freed only once every request that held it
// is done. The server adds one to each request's context (`with_checker_data_lease`) and makes it the thread's lease
// while the handler runs (`enter_checker_data_lease`: call hierarchy and file rename take checkers with
// `context.Background()`, as in Go); it ends when the last clone of that context and the last scope are dropped,
// after the handler returned.
#[derive(Default)]
struct CheckerDataLease {
    held: Mutex<Vec<(Weak<CheckerPoolImpl>, usize)>>,
}

// The context value (`Context::value` hands out references; the thread's lease needs its own handle).
struct CheckerDataLeaseKey(Arc<CheckerDataLease>);

thread_local! {
    static THREAD_LEASE: std::cell::RefCell<Option<Arc<CheckerDataLease>>> = const { std::cell::RefCell::new(None) };
}

// While alive, checkers this thread takes with a context that has no lease get the lease of `ctx`
// (`enter_checker_data_lease`).
pub struct CheckerDataLeaseScope {
    saved: Option<Arc<CheckerDataLease>>,
    _not_send: std::marker::PhantomData<*const ()>,
}

impl Drop for CheckerDataLeaseScope {
    fn drop(&mut self) {
        let saved = self.saved.take();
        THREAD_LEASE.with(|l| *l.borrow_mut() = saved);
    }
}

pub fn enter_checker_data_lease(ctx: &Context) -> CheckerDataLeaseScope {
    let lease = ctx.value::<CheckerDataLeaseKey>().map(|k| Arc::clone(&k.0));
    let saved = THREAD_LEASE.with(|l| std::mem::replace(&mut *l.borrow_mut(), lease));
    CheckerDataLeaseScope { saved, _not_send: std::marker::PhantomData }
}

impl Drop for CheckerDataLease {
    fn drop(&mut self) {
        for (pool, key) in std::mem::take(self.held.get_mut().unwrap()) {
            if let Some(pool) = pool.upgrade() {
                pool.end_lease(key);
            }
        }
    }
}

// A context whose holders get a lease (see `checkerDataLease`).
pub fn with_checker_data_lease(ctx: &Context) -> Context {
    ctx.with_value(CheckerDataLeaseKey(Arc::new(CheckerDataLease::default())))
}

// mergeGlobalDiagnosticsFromCheckerLocked's merge. Region hook: the pool keeps the regions of the diagnostics it
// accumulates, including those a merged copy refers to (`compact_and_merge_related_infos` clones a diagnostic outside
// the checker's region, sharing its text, chain and related information).
pub fn accumulate_global_diagnostics(all: &[P<Diagnostic>]) -> Vec<P<Diagnostic>> {
    let accumulated = sort_and_deduplicate_diagnostics(all);
    for &d in &accumulated {
        tsrs_ast::note_diagnostic_escape(d);
    }
    accumulated
}

// checkerpool.go:84
pub fn new_checker_pool(
    mut opts: CheckerPoolOptions,
    program: &'static Program,
    log: Option<Box<dyn Fn(&str) + Send + Sync>>,
) -> Arc<CheckerPoolImpl> {
    if opts.max_checkers == 0 {
        opts.max_checkers = 4;
    } else if opts.max_checkers < 2 {
        opts.max_checkers = 2; // at least 1 diagnostics + 1 query checker
    }
    if opts.idle_timeout.is_zero() {
        opts.idle_timeout = Duration::from_secs(30);
    }
    let query_slots = opts.max_checkers - 1;
    let log = log.unwrap_or_else(|| Box::new(|_msg: &str| {}));
    Arc::new_cyclic(|self_ref| CheckerPoolImpl {
        program,
        opts,
        mu: Mutex::new(CheckerPoolState {
            discarded: false,
            checkers: std::iter::repeat_with(|| None).take(opts.max_checkers).collect(),
            held_by: vec![String::new(); opts.max_checkers],
            file_associations: FxHashMap::default(),
            request_associations: FxHashMap::default(),
            last_released: vec![None; opts.max_checkers],
            cleanup_timer: None,
            persistent_checker: None,
            persistent_held: false,
            global_diag_accumulated: Vec::new(),
            global_diag_changed: false,
            global_diag_checker_count: vec![0; opts.max_checkers],
        }),
        diag_sem: Semaphore::new(1),
        query_sem: Semaphore::new(query_slots),
        persistent_sem: Semaphore::new(1),
        log,
        self_ref: Weak::clone(self_ref),
        parked: Mutex::new(Vec::new()),
        instances: Mutex::new(FxHashMap::default()),
        owner: Mutex::new(None),
    })
}

// checkerpool.go:116
// holdTag returns the value to store in heldBy for the given request ID.
fn hold_tag(request_id: &str) -> String {
    if request_id.is_empty() {
        return CHECKER_HELD_ANONYMOUS.to_string();
    }
    request_id.to_string()
}

// The `CheckerPool` the program calls (Go: `*checkerPool` itself).
pub struct CheckerPoolHandle(pub(crate) Arc<CheckerPoolImpl>);

impl CheckerPool for CheckerPoolHandle {
    fn get_checker(&self, ctx: &Context, file: Option<P<SourceFile>>) -> CheckerHandle {
        self.0.get_checker(ctx, file)
    }
}

impl CheckerPoolImpl {
    // Region hook: a new checker is created inside its own region. `pinned`: see `checkerInstance`.
    fn new_pooled_checker(&self, pinned: bool) -> PooledChecker {
        let region = Region::new(1 << 20);
        let mut checker = {
            let _scope = region.enter();
            PooledChecker::new(tsrs_checker::new_checker(self.program))
        };
        self.instances.lock().unwrap().insert(
            checker.as_non_null().as_ptr() as usize,
            CheckerInstance { disposed: None, region, leases: 0, pinned },
        );
        checker
    }

    // Region hook: while a checker is held, its region is the holder's allocation target and the program's owner
    // is kept alive. The holder's request takes a lease on the checker's data (`checkerDataLease`, from `ctx` or the
    // thread); a holder without one pins the checker. `ctx` is None for the API checker, which is always pinned.
    fn enter_checker_region(
        &self,
        ctx: Option<&Context>,
        c: std::ptr::NonNull<Checker>,
    ) -> (Option<RegionScope>, Option<Arc<ProgramOwner>>) {
        let key = c.as_ptr() as usize;
        let lease = ctx.and_then(|ctx| {
            ctx.value::<CheckerDataLeaseKey>()
                .map(|k| Arc::clone(&k.0))
                .or_else(|| THREAD_LEASE.with(|l| l.borrow().clone()))
        });
        let region = {
            let mut instances = self.instances.lock().unwrap();
            let instance = instances.get_mut(&key);
            if let Some(instance) = instance {
                match lease {
                    Some(lease) => {
                        let mut held = lease.held.lock().unwrap();
                        if !held
                            .iter()
                            .any(|(pool, k)| *k == key && std::ptr::eq(pool.as_ptr(), self))
                        {
                            held.push((Weak::clone(&self.self_ref), key));
                            instance.leases += 1;
                        }
                    }
                    None => instance.pinned = true,
                }
                Some(instance.region.clone())
            } else {
                None
            }
        };
        (region.map(|r| r.enter()), self.live_owner())
    }

    // A lease on checker `key` ended (`checkerDataLease`): frees the checker if it was disposed and this was its last.
    fn end_lease(&self, key: usize) {
        let mut instances = self.instances.lock().unwrap();
        let Some(instance) = instances.get_mut(&key) else {
            return;
        };
        instance.leases -= 1;
        if instance.leases > 0 || instance.disposed.is_none() {
            return;
        }
        let instance = instances.remove(&key).unwrap();
        drop(instances);
        self.free_or_park(key, instance);
    }

    // Frees a disposed checker and its region, or parks it until the pool is freed if its data may be referenced
    // from longer-lived structures (see `parked`).
    fn free_or_park(&self, key: usize, mut instance: CheckerInstance) {
        if instance.pinned || instance.region.escaped() {
            log_region(|| {
                format!(
                    "checker {key:#x}: parked (pinned {}, escaped {})",
                    instance.pinned,
                    instance.region.escaped()
                )
            });
            self.parked.lock().unwrap().extend(instance.disposed.take());
            self.instances.lock().unwrap().insert(key, instance);
            return;
        }
        log_region(|| {
            format!(
                "checker {key:#x}: freed (region {} KiB, {} KiB used)",
                instance.region.allocated_bytes() >> 10,
                instance.region.used_bytes() >> 10
            )
        });
        drop(instance);
    }

    pub(crate) fn set_owner(&self, owner: Weak<ProgramOwner>) {
        *self.owner.lock().unwrap() = Some(owner);
    }

    fn live_owner(&self) -> Option<Arc<ProgramOwner>> {
        (*self.owner.lock().unwrap())
            .as_ref()
            .map(|w| w.upgrade().expect("checker pool used after its program was freed"))
    }

    // Region hook: frees every checker of the pool and their regions. Called when the program is freed (nothing
    // holds a checker or refers to checker data any more).
    pub fn free_checkers(&self) {
        let mut checkers: Vec<PooledChecker> = std::mem::take(&mut *self.parked.lock().unwrap());
        {
            let mut st = self.mu.lock().unwrap();
            checkers.extend(st.checkers.iter_mut().filter_map(Option::take));
            checkers.extend(st.persistent_checker.take());
            st.global_diag_accumulated.clear();
        }
        let mut instances = std::mem::take(&mut *self.instances.lock().unwrap());
        checkers.extend(instances.values_mut().filter_map(|instance| instance.disposed.take()));
        drop(checkers);
        drop(instances);
    }

    fn arc(&self) -> Arc<CheckerPoolImpl> {
        self.self_ref.upgrade().expect("checker pool used after it was dropped")
    }

    // checkerpool.go:123
    pub fn get_checker(&self, ctx: &Context, file: Option<P<SourceFile>>) -> CheckerHandle {
        drop(self.live_owner());
        let lifetime = get_checker_lifetime(ctx);
        let mut request_id = get_request_id(ctx).to_string();

        // Request affinity is cleaned up via context.AfterFunc when the request
        // context is done. If the context can never be canceled (ctx.Done() == nil,
        // e.g. context.Background()), that cleanup would never run and
        // requestAssociations would grow unboundedly, so disable affinity entirely.
        if ctx.done_is_nil() {
            request_id = String::new();
        }

        match lifetime {
            CheckerLifetime::Diagnostics => self.get_diagnostics_checker(ctx, &request_id),
            CheckerLifetime::API => self.get_persistent_checker(),
            CheckerLifetime::Temporary => self.get_query_checker(ctx, &request_id, file),
        }
    }

    // checkerpool.go:158
    // tryReacquireForRequest claims a semaphore slot, then checks whether the given
    // request has an idle associated checker. The caller must provide the
    // appropriate semaphore channel and indicate whether this is a diagnostics
    // request (isDiag). If the associated checker is in the wrong category
    // (e.g. a diagnostics index for a query request), the association is deleted
    // and normal acquisition proceeds.
    //
    // Request affinity is only a preference for an idle checker, not permission to
    // reuse a held checker: concurrent acquisitions can share the same request ID.
    // Returns (checker, release, true) if the checker was reclaimed.
    // Returns (nil, nil, false) if the caller must proceed with
    // normal acquisition — in this case, a semaphore slot has already been claimed.
    // Must NOT be called with p.mu held.
    fn try_reacquire_for_request(
        &self,
        ctx: &Context,
        request_id: &str,
        sem: &Semaphore,
        is_diag: bool,
    ) -> Option<CheckerHandle> {
        sem.acquire();
        if request_id.is_empty() {
            return None;
        }

        let mut st = self.mu.lock().unwrap();
        let index = *st.request_associations.get(request_id)?;

        // Validate that the associated index matches the expected category.
        // Index 0 is for diagnostics; indices 1+ are for queries.
        if (is_diag && index != 0) || (!is_diag && index == 0) {
            st.request_associations.remove(request_id);
            return None;
        }

        if st.checkers[index].is_none() {
            st.request_associations.remove(request_id);
            return None;
        }

        if st.held_by[index].is_empty() {
            st.held_by[index] = request_id.to_string();
            return Some(self.create_release(ctx, &mut st, request_id, index));
        }

        None
    }

    // checkerpool.go:194
    // getDiagnosticsChecker returns the dedicated diagnostics checker (index 0).
    // Creates it on first use. Blocks on diagSem if it's currently in use.
    fn get_diagnostics_checker(&self, ctx: &Context, request_id: &str) -> CheckerHandle {
        const DIAG_INDEX: usize = 0;

        if let Some(handle) = self.try_reacquire_for_request(ctx, request_id, &self.diag_sem, true)
        {
            return handle;
        }

        // Token consumed — proceed with normal acquisition.
        let mut st = self.mu.lock().unwrap();

        if st.checkers[DIAG_INDEX].is_none() {
            (self.log)("checkerpool: Creating diagnostics checker");
            st.checkers[DIAG_INDEX] = Some(self.new_pooled_checker(false));
        }

        st.held_by[DIAG_INDEX] = hold_tag(request_id);
        (self.log)(&format!(
            "checkerpool: Acquired diagnostics checker for request {}",
            hold_tag(request_id)
        ));
        if !request_id.is_empty() && !st.request_associations.contains_key(request_id) {
            st.request_associations.insert(request_id.to_string(), DIAG_INDEX);
            self.register_request_cleanup(ctx, request_id);
        }
        self.create_release(ctx, &mut st, request_id, DIAG_INDEX)
    }

    // checkerpool.go:226
    // getQueryChecker returns an ephemeral query checker from indices 1+.
    // Uses request affinity, then file affinity, then finds/creates.
    // Blocks on querySem if all query slots are in use.
    fn get_query_checker(
        &self,
        ctx: &Context,
        request_id: &str,
        file: Option<P<SourceFile>>,
    ) -> CheckerHandle {
        if let Some(handle) =
            self.try_reacquire_for_request(ctx, request_id, &self.query_sem, false)
        {
            return handle;
        }

        // Token consumed — proceed with normal acquisition.
        let mut st = self.mu.lock().unwrap();

        // Try file affinity.
        if let Some(file) = file
            && let Some(&index) = st.file_associations.get(&file)
            && index > 0
            && st.checkers[index].is_some()
            && st.held_by[index].is_empty()
        {
            st.held_by[index] = hold_tag(request_id);
            if !request_id.is_empty() && !st.request_associations.contains_key(request_id) {
                st.request_associations.insert(request_id.to_string(), index);
                self.register_request_cleanup(ctx, request_id);
            }
            return self.create_release(ctx, &mut st, request_id, index);
        }

        // Find any available query checker or create one.
        let index = self.find_or_create_query_checker_locked(&mut st);
        st.held_by[index] = hold_tag(request_id);
        (self.log)(&format!(
            "checkerpool: Acquired query checker {} for request {}",
            index,
            hold_tag(request_id)
        ));
        if !request_id.is_empty() && !st.request_associations.contains_key(request_id) {
            st.request_associations.insert(request_id.to_string(), index);
            self.register_request_cleanup(ctx, request_id);
        }
        if let Some(file) = file {
            st.file_associations.insert(file, index);
        }
        self.create_release(ctx, &mut st, request_id, index)
    }

    // checkerpool.go:270
    // findOrCreateQueryCheckerLocked returns an idle query checker or creates one
    // in the first empty slot. The semaphore guarantees at least one slot is
    // available. Must be called with p.mu held.
    fn find_or_create_query_checker_locked(&self, st: &mut CheckerPoolState) -> usize {
        // Prefer an existing idle checker.
        for i in 1..st.checkers.len() {
            if st.checkers[i].is_some() && st.held_by[i].is_empty() {
                return i;
            }
        }
        // Create in the first empty slot.
        for (i, element) in st.checkers.iter_mut().enumerate().skip(1) {
            if (*element).is_none() {
                (self.log)(&format!("checkerpool: Creating query checker {i}"));
                (*element) = Some(self.new_pooled_checker(false));
                return i;
            }
        }
        panic!("checkerpool: no available query slot despite holding semaphore token");
    }

    // checkerpool.go:289
    fn get_persistent_checker(&self) -> CheckerHandle {
        self.persistent_sem.acquire();
        let mut st = self.mu.lock().unwrap();

        if st.persistent_checker.is_none() {
            (self.log)("checkerpool: Creating persistent checker");
            // Pinned: API clients keep handles to its types and symbols across requests.
            st.persistent_checker = Some(self.new_pooled_checker(true));
        }

        let c = st.persistent_checker.as_mut().unwrap().as_non_null();
        st.persistent_held = true;
        drop(st);

        let (region_scope, owner) = self.enter_checker_region(None, c);
        let p = self.arc();
        let release = move || {
            drop(region_scope);
            let _owner = owner;
            let mut st = p.mu.lock().unwrap();
            st.persistent_held = false;
            let canceled = st
                .persistent_checker
                .as_mut()
                .is_some_and(|pc| pc.as_non_null() == c && pc.was_canceled());
            if canceled {
                // A canceled checker panics on reuse, so drop it; the next API
                // acquisition will create a fresh persistent checker.
                (p.log)("checkerpool: Persistent checker was canceled, disposing");
                let old = st.persistent_checker.take();
                drop(st);
                if let Some(old) = old {
                    p.dispose_checker(old);
                }
            } else {
                drop(st);
            }
            p.persistent_sem.release();
        };
        // SAFETY: the persistent checker is held (`persistentHeld`, one semaphore slot) until `release` runs; the
        // pool keeps it alive in `persistent_checker` until then.
        unsafe { CheckerHandle::from_raw(c, release) }
    }

    // checkerpool.go:319 (`ctx`: the holder's context, for its lease)
    fn create_release(
        &self,
        ctx: &Context,
        st: &mut CheckerPoolState,
        request_id: &str,
        index: usize,
    ) -> CheckerHandle {
        let c = st.checkers[index].as_mut().unwrap().as_non_null();
        let (region_scope, owner) = self.enter_checker_region(Some(ctx), c);
        let p = self.arc();
        let request_id = request_id.to_string();
        let release = move || {
            drop(region_scope);
            let _owner = owner;
            let mut st = p.mu.lock().unwrap();

            let was_canceled = st.checkers[index].as_mut().unwrap().was_canceled();
            let mut disposed = None;
            if was_canceled {
                // Canceled checkers must be disposed.
                (p.log)(&format!(
                    "checkerpool: Checker {} for request {} was canceled, disposing",
                    index,
                    hold_tag(&request_id)
                ));
                disposed = CheckerPoolImpl::dispose_checker_locked(&mut st, index, c);
            } else {
                // Query checkers can produce incidental errors while serializing types.
                if index == 0 {
                    CheckerPoolImpl::merge_global_diagnostics_from_checker_locked(&mut st, index);
                }
                st.held_by[index] = String::new();
                st.last_released[index] = Some(Instant::now());
                if !st.discarded {
                    p.schedule_cleanup_locked(&mut st);
                }
                // If discarded, skip scheduling cleanup — checkers stay alive
                // until the pool is garbage collected so that API clients can
                // continue resolving type/symbol handles.
            }

            // Unlock before releasing the semaphore slot. If we received from
            // the channel while holding p.mu, a woken goroutine could immediately
            // try to acquire p.mu, risking priority inversion or unnecessary
            // contention.
            drop(st);
            if let Some(checker) = disposed {
                p.dispose_checker(checker);
            }

            // Release the semaphore slot.
            if index == 0 {
                p.diag_sem.release();
            } else {
                p.query_sem.release();
            }
        };
        // SAFETY: checker `index` is marked held (`heldBy[index]`) until `release` runs, and held checkers are never
        // disposed by anyone else, so the pointer stays valid.
        unsafe { CheckerHandle::from_raw(c, release) }
    }

    // checkerpool.go:361
    // registerRequestCleanup uses context.AfterFunc to delete the request
    // association when the request context is done. This prevents the map
    // from growing unboundedly with completed request IDs.
    // Must be called with p.mu held; the cleanup runs asynchronously.
    fn register_request_cleanup(&self, ctx: &Context, request_id: &str) {
        let p = Weak::clone(&self.self_ref);
        let request_id = request_id.to_string();
        ctx.after_func(move || {
            if let Some(p) = p.upgrade() {
                p.mu.lock().unwrap().request_associations.remove(&request_id);
            }
        });
    }

    // checkerpool.go:373
    // scheduleCleanupLocked resets (or starts) the cleanup timer so it fires at
    // the earliest pending checker-expiration deadline among all currently idle,
    // unheld checkers.
    // Must be called with p.mu held. Must NOT be called on discarded pools.
    fn schedule_cleanup_locked(&self, st: &mut CheckerPoolState) {
        let mut earliest_deadline: Option<Instant> = None;
        for i in 0..st.checkers.len() {
            let Some(last_released) = st.last_released[i] else {
                continue;
            };
            if st.checkers[i].is_none() || !st.held_by[i].is_empty() {
                continue;
            }
            let deadline = last_released + self.opts.idle_timeout;
            if earliest_deadline.is_none_or(|earliest| deadline < earliest) {
                earliest_deadline = Some(deadline);
            }
        }
        let Some(earliest_deadline) = earliest_deadline else {
            // No idle checkers remain — stop the timer if it exists.
            if let Some(timer) = st.cleanup_timer.take() {
                timer.stop();
            }
            return;
        };
        let mut delay = earliest_deadline.saturating_duration_since(Instant::now());
        if delay.is_zero() {
            delay = Duration::from_millis(1);
        }
        if let Some(timer) = &st.cleanup_timer {
            timer.reset(delay);
        } else {
            let p = Weak::clone(&self.self_ref);
            st.cleanup_timer = Some(after_func(delay, move || {
                if let Some(p) = p.upgrade() {
                    p.cleanup_idle_checkers();
                }
            }));
        }
    }

    // checkerpool.go:405
    // cleanupIdleCheckers disposes checkers that have been idle for longer than
    // the idle timeout. The API checker is separate and never idle-cleaned.
    fn cleanup_idle_checkers(&self) {
        // Region hook: the program (and so the checkers' regions) must outlive the disposals below.
        let owner = (*self.owner.lock().unwrap()).as_ref().map(Weak::upgrade);
        if matches!(owner, Some(None)) {
            return;
        }
        let mut st = self.mu.lock().unwrap();
        // The timer callback may already have been in flight when Discard() called
        // Stop() (which does not guarantee the callback won't run). Bail out without
        // rescheduling so a discarded pool doesn't keep itself alive via a new timer.
        if st.discarded {
            return;
        }
        let now = Instant::now();
        let mut disposed = Vec::new();
        for i in 0..st.checkers.len() {
            if st.checkers[i].is_none() || !st.held_by[i].is_empty() {
                continue;
            }
            let Some(last_released) = st.last_released[i] else {
                continue;
            };
            let idle = now.duration_since(last_released);
            if idle >= self.opts.idle_timeout {
                (self.log)(&format!("checkerpool: Disposing idle checker {i} (idle {idle:?})"));
                let c = st.checkers[i].as_mut().unwrap().as_non_null();
                disposed.extend(Self::dispose_checker_locked(&mut st, i, c));
            }
        }
        // Reschedule for any remaining idle-but-not-yet-expired checkers.
        // scheduleCleanupLocked will Reset the existing timer rather than
        // creating a new one, avoiding goroutine leaks.
        self.schedule_cleanup_locked(&mut st);
        drop(st);
        for checker in disposed {
            self.dispose_checker(checker);
        }
    }

    // checkerpool.go:437
    // disposeCheckerLocked removes a checker from the pool and clears all associations
    // (file and request) that reference it. Must be called with p.mu held.
    // Region hook: returns the checker, for `dispose_checker` once p.mu is released.
    #[must_use]
    fn dispose_checker_locked(
        st: &mut CheckerPoolState,
        index: usize,
        c: std::ptr::NonNull<Checker>,
    ) -> Option<PooledChecker> {
        assert!(st.checkers[index].as_mut().is_some_and(|pc| pc.as_non_null() == c));
        let old = st.checkers[index].take();
        st.held_by[index] = String::new();
        st.global_diag_checker_count[index] = 0;
        st.last_released[index] = None;
        st.file_associations.retain(|_, idx| *idx != index);
        st.request_associations.retain(|_, idx| *idx != index);
        old
    }

    // checkerpool.go:458
    // mergeGlobalDiagnosticsFromCheckerLocked checks if the given checker has produced new global
    // diagnostics since the last time we looked, and if so merges them into the accumulated set.
    // Must be called with p.mu held.
    fn merge_global_diagnostics_from_checker_locked(st: &mut CheckerPoolState, index: usize) {
        let globals = st.checkers[index].as_mut().unwrap().get_global_diagnostics();
        if globals.len() == st.global_diag_checker_count[index] {
            return;
        }
        st.global_diag_checker_count[index] = globals.len();
        let before = st.global_diag_accumulated.len();
        let mut all = std::mem::take(&mut st.global_diag_accumulated);
        all.extend(globals);
        st.global_diag_accumulated = accumulate_global_diagnostics(&all);
        if st.global_diag_accumulated.len() != before {
            st.global_diag_changed = true;
        }
    }

    // checkerpool.go:473
    // GetGlobalDiagnostics returns the global diagnostics accumulated from the dedicated
    // diagnostics checker across its instances during this pool's lifetime.
    pub fn get_global_diagnostics(&self) -> Vec<P<Diagnostic>> {
        self.mu.lock().unwrap().global_diag_accumulated.clone()
    }

    // checkerpool.go:481
    // TakeNewGlobalDiagnostics reports whether new global diagnostics have been
    // accumulated since the last call, and resets the flag.
    pub(crate) fn take_new_global_diagnostics(&self) -> bool {
        let mut st = self.mu.lock().unwrap();
        std::mem::replace(&mut st.global_diag_changed, false)
    }

    // checkerpool.go:493
    // Discard signals that this pool's program has been replaced. The pool
    // remains functional but stops its idle-cleanup timer so that checkers
    // are not disposed until the pool is GC'd. The API checker is unaffected
    // since it is never idle-cleaned.
    pub fn discard(&self) {
        let mut st = self.mu.lock().unwrap();
        if st.discarded {
            return; // already discarded
        }
        (self.log)("checkerpool: Discarding pool, stopping idle cleanup");
        st.discarded = true;
        if let Some(timer) = st.cleanup_timer.take() {
            timer.stop();
        }
    }

    pub fn checker_ptr(&self, index: usize) -> Option<*const Checker> {
        self.mu.lock().unwrap().checkers[index].as_ref().map(|c| &raw const **c)
    }

    pub fn persistent_ptr(&self) -> Option<*const Checker> {
        self.mu.lock().unwrap().persistent_checker.as_ref().map(|c| &raw const **c)
    }

    pub fn opts(&self) -> CheckerPoolOptions {
        self.opts
    }

    pub fn query_sem_used(&self) -> usize {
        self.query_sem.state.lock().unwrap().used
    }

    // (checker regions, disposed checkers waiting for their leases, parked checkers)
    pub fn region_counts(&self) -> (usize, usize, usize) {
        let instances = self.instances.lock().unwrap();
        (
            instances.len(),
            instances.values().filter(|i| i.disposed.is_some()).count(),
            self.parked.lock().unwrap().len(),
        )
    }

    pub fn test_state<R>(&self, f: impl FnOnce(&CheckerPoolTestView) -> R) -> R {
        let st = self.mu.lock().unwrap();
        let view = CheckerPoolTestView {
            checkers: st.checkers.iter().map(std::option::Option::is_some).collect(),
            held_by: st.held_by.clone(),
            file_associations: st.file_associations.clone(),
            request_associations: st.request_associations.clone(),
            has_cleanup_timer: st.cleanup_timer.is_some(),
            discarded: st.discarded,
            persistent: st.persistent_checker.is_some(),
            persistent_held: st.persistent_held,
        };
        f(&view)
    }
}

pub struct CheckerPoolTestView {
    pub checkers: Vec<bool>,
    #[expect(
        dead_code,
        reason = "Retained in the Go-shaped pool test view; the currently ported cases inspect other fields."
    )]
    held_by: Vec<String>,
    pub file_associations: FxHashMap<P<SourceFile>, usize>,
    pub request_associations: FxHashMap<String, usize>,
    pub has_cleanup_timer: bool,
    pub discarded: bool,
    pub persistent: bool,
    #[expect(
        dead_code,
        reason = "Retained in the Go-shaped pool test view; the currently ported cases inspect other fields."
    )]
    persistent_held: bool,
}

impl CheckerPoolImpl {
    // Go drops the pool's reference and lets the GC reclaim the checker. Region hook: the checker and its region are
    // freed now if no lease is left, when the last lease ends otherwise, or with the pool if its data may be
    // referenced from longer-lived structures (see `parked`). Must NOT be called with p.mu held.
    fn dispose_checker(&self, mut checker: PooledChecker) {
        let key = checker.as_non_null().as_ptr() as usize;
        let mut instances = self.instances.lock().unwrap();
        let instance = instances.get_mut(&key).expect("disposed checker has no region");
        instance.disposed = Some(checker);
        if instance.leases > 0 {
            return;
        }
        let instance = instances.remove(&key).unwrap();
        drop(instances);
        self.free_or_park(key, instance);
    }
}
