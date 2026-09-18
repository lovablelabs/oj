// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! In-process JavaScript engine for oj, backed by Deno (deno_core +
//! deno_runtime) with Node compatibility over the app's own node_modules.
//!
//! A [`JsEngine`] is a handle to a dedicated OS thread that owns a
//! `MainWorker` (a V8 isolate is `!Send`, so it can never leave that thread).
//! The handle itself is `Send + Sync`: callers submit jobs over a channel and
//! await the reply. One isolate per thread only -- V8 aborts the process when
//! two runtimes are dropped on the same thread.

mod bridge;
mod code_cache;
mod host;
mod loader;
mod worker;

pub use bridge::EngineHooks;
pub use bridge::RpcHandler;
pub use code_cache::engine_abi_key;
/// Native addons that would re-initialize into dangling process-global state
/// if a new engine loaded them now (every runtime that registered them is
/// gone) — a pre-3.10 napi-rs addon can crash the process on that. Embedders
/// gate in-process engine respawns on this being empty.
pub use deno_napi::addons_pending_unsafe_reregistration;
/// Native addons some live engine currently holds, for a keeper env that
/// pre-registers them before a dying engine's teardown can orphan them.
pub use deno_napi::addons_with_live_registrations;
pub use host::HostFuture;
pub use host::HostModule;
pub use host::HostModuleType;
pub use host::HostResolved;
pub use host::ModuleHost;

use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Once;
use std::time::Duration;

use deno_core::url::Url;
use deno_core::v8;
use deno_core::PollEventLoopOptions;
use deno_runtime::worker::MainWorker;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

/// Configuration for a [`JsEngine`].
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// The app root. Bare specifiers resolve from this directory's
    /// node_modules (walking up, like Node), and relative eval paths are
    /// joined onto it.
    pub root: PathBuf,
    /// Hard cap on the V8 heap. Exceeding it terminates the running job with
    /// [`EngineError::MemoryLimit`] instead of aborting the process.
    pub memory_limit_bytes: Option<usize>,
    /// Default wall-clock deadline applied to every job that does not carry
    /// its own.
    pub default_deadline: Option<Duration>,
    /// Persistent V8 code-cache directory. When set, compiled bytecode for
    /// the modules an engine loads from disk (ESM, `require`d CJS, residual
    /// ext scripts) is stored here and reused by later engines, cutting the
    /// repeat parse/compile of a large unchanging toolchain. The caller keys
    /// the directory by its own version; entries self-invalidate on source
    /// change through an embedded source hash. Best-effort: a broken or
    /// read-only cache only costs the speedup.
    pub code_cache_dir: Option<PathBuf>,
}

impl EngineConfig {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            memory_limit_bytes: None,
            default_deadline: None,
            code_cache_dir: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("failed to boot JS engine: {0}")]
    Boot(String),
    #[error("{0}")]
    Js(String),
    #[error("JS execution exceeded the memory limit")]
    MemoryLimit,
    #[error("JS execution exceeded the deadline")]
    Deadline,
    #[error("the JS engine is shut down")]
    Closed,
}

/// What an eval job executes.
#[derive(Debug)]
pub enum EvalInput {
    /// An ES module given as source text.
    Source(String),
    /// An ES module on disk; relative paths are joined onto the engine root.
    Path(PathBuf),
}

type Reply = oneshot::Sender<Result<serde_json::Value, EngineError>>;

/// Kept extensible: later PRs add typed ops and long-lived job kinds.
enum Job {
    /// Execute an ES module to completion (event loop drained). Replies with
    /// the module's default export when it is JSON-serializable, else `null`.
    Eval {
        input: EvalInput,
        deadline: Option<Duration>,
        reply: Reply,
    },
    /// Execute a module, then call one of its exports with JSON arguments.
    /// A returned promise is resolved before replying. Call jobs run
    /// concurrently: while one call's promise is pending (a fetch, a timer),
    /// the engine accepts and progresses other jobs on the same isolate.
    Call {
        module: String,
        export: String,
        args: Vec<serde_json::Value>,
        deadline: Option<Duration>,
        reply: Reply,
    },
}

/// Handle to an engine thread. Dropping it shuts the thread down gracefully
/// (the job channel closes, the loop ends, the thread is joined) — unless the
/// engine was [`JsEngine::abandon`]ed first, which detaches instead of joining.
pub struct JsEngine {
    tx: Mutex<Option<mpsc::UnboundedSender<Job>>>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// The isolate's thread-safe handle, for interrupting a running job from
    /// outside the engine thread (see [`JsEngine::abandon`]).
    isolate: v8::IsolateHandle,
    default_deadline: Option<Duration>,
}

impl JsEngine {
    pub fn spawn(config: EngineConfig) -> Result<JsEngine, EngineError> {
        Self::spawn_inner(config, None, None)
    }

    /// Spawns an engine whose module loading is governed by `host` (see
    /// [`ModuleHost`]). Must be called from inside a tokio runtime: host
    /// futures run on that runtime, never on the isolate thread.
    pub fn spawn_with_host(
        config: EngineConfig,
        module_host: Arc<dyn ModuleHost>,
    ) -> Result<JsEngine, EngineError> {
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| {
            EngineError::Boot("spawn_with_host must be called from inside a tokio runtime".into())
        })?;
        Self::spawn_inner(config, Some(host::HostBridge::new(runtime, module_host)), None)
    }

    /// Spawns an engine with JS→Rust bridges installed as globals before any
    /// module runs (see [`EngineHooks`]): the in-process plugin host's push
    /// channel (`__oj_post`) and synchronous ctx-RPC (`__oj_rpc`).
    pub fn spawn_with_hooks(
        config: EngineConfig,
        hooks: EngineHooks,
    ) -> Result<JsEngine, EngineError> {
        Self::spawn_inner(config, None, Some(hooks))
    }

    fn spawn_inner(
        config: EngineConfig,
        module_host: Option<host::HostBridge>,
        hooks: Option<EngineHooks>,
    ) -> Result<JsEngine, EngineError> {
        init_v8_platform_once();
        let default_deadline = config.default_deadline;
        let (tx, rx) = mpsc::unbounded_channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("oj-js-engine".into())
            // V8 + deeply recursive module instantiation want more than the
            // 2MB default, especially in debug builds.
            .stack_size(8 * 1024 * 1024)
            .spawn(move || engine_thread(config, module_host, hooks, rx, ready_tx))
            .map_err(|e| EngineError::Boot(e.to_string()))?;
        match ready_rx.recv() {
            Ok(Ok(isolate)) => Ok(JsEngine {
                tx: Mutex::new(Some(tx)),
                thread: Mutex::new(Some(thread)),
                isolate,
                default_deadline,
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err(EngineError::Boot(
                    "engine thread exited during startup".into(),
                ))
            }
        }
    }

    /// Gives up on the engine without waiting for it: terminates whatever JS
    /// is running (a wedged synchronous hook unwinds and the thread then exits
    /// by itself on the closed channel), closes the job channel, and detaches
    /// the thread so no caller ever joins it. A job wedged in NATIVE code
    /// (a napi call, a blocking child wait; not `Atomics.wait`, which V8's
    /// terminate interrupts) cannot be interrupted: that thread — and its
    /// isolate — leak until the block ends, which is the accepted cost of
    /// abandoning in-process what a process kill used to reclaim.
    pub fn abandon(&self) {
        self.isolate.terminate_execution();
        self.tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        // Dropping the JoinHandle detaches the thread; Drop will find None.
        self.thread
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }

    /// Executes an ES module to completion with the engine's default deadline.
    pub async fn eval(&self, input: EvalInput) -> Result<serde_json::Value, EngineError> {
        self.eval_with_deadline(input, self.default_deadline).await
    }

    /// Executes an ES module to completion with an explicit deadline
    /// (`None` disables the deadline for this job).
    pub async fn eval_with_deadline(
        &self,
        input: EvalInput,
        deadline: Option<Duration>,
    ) -> Result<serde_json::Value, EngineError> {
        self.request(|reply| Job::Eval {
            input,
            deadline,
            reply,
        })
        .await
    }

    /// Executes `module` (a path, or an absolute module URL), then calls its
    /// `export` with `args` (JSON in, JSON out). A returned promise is
    /// resolved before replying; calls run concurrently on the isolate.
    pub async fn call(
        &self,
        module: impl Into<String>,
        export: &str,
        args: Vec<serde_json::Value>,
    ) -> Result<serde_json::Value, EngineError> {
        self.call_with_deadline(module, export, args, self.default_deadline)
            .await
    }

    /// [`JsEngine::call`] with an explicit deadline (`None` disables it).
    ///
    /// Through the call's exclusive setup phase (module evaluation + the
    /// synchronous part of the invocation — no other JS can be on the stack)
    /// a watchdog terminates a wedged run. Once the call parks on its
    /// returned promise the scheduler abandons the still-pending promise at
    /// the deadline, replying [`EngineError::Deadline`] while the isolate —
    /// and every concurrent call — carries on untouched; the watchdog stays
    /// armed underneath solely for a continuation that wedges the event loop
    /// in a busy loop, where only a termination gets the isolate back.
    pub async fn call_with_deadline(
        &self,
        module: impl Into<String>,
        export: &str,
        args: Vec<serde_json::Value>,
        deadline: Option<Duration>,
    ) -> Result<serde_json::Value, EngineError> {
        let module = module.into();
        let export = export.to_string();
        self.request(|reply| Job::Call {
            module,
            export,
            args,
            deadline,
            reply,
        })
        .await
    }

    async fn request(
        &self,
        make_job: impl FnOnce(Reply) -> Job,
    ) -> Result<serde_json::Value, EngineError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        {
            let tx = self
                .tx
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            tx.as_ref()
                .ok_or(EngineError::Closed)?
                .send(make_job(reply_tx))
                .map_err(|_| EngineError::Closed)?;
        }
        reply_rx.await.map_err(|_| EngineError::Closed)?
    }
}

impl Drop for JsEngine {
    fn drop(&mut self) {
        // Close the channel first so the engine thread's recv loop ends.
        self.tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let thread = self
            .thread
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(thread) = thread {
            let _ = thread.join();
        }
    }
}

fn init_v8_platform_once() {
    static V8_INIT: Once = Once::new();
    // init_platform must run exactly once process-wide, before the first
    // isolate on any thread.
    V8_INIT.call_once(|| deno_core::JsRuntime::init_platform(None));
}

fn engine_thread(
    config: EngineConfig,
    module_host: Option<host::HostBridge>,
    hooks: Option<EngineHooks>,
    mut rx: mpsc::UnboundedReceiver<Job>,
    ready: std::sync::mpsc::Sender<Result<v8::IsolateHandle, EngineError>>,
) {
    // Current-thread runtime: the JsRuntime is !Send and every part of the
    // worker must stay on this thread.
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = ready.send(Err(EngineError::Boot(e.to_string())));
            return;
        }
    };

    rt.block_on(async move {
        let root_url = match deno_path_util::url_from_directory_path(&config.root) {
            Ok(url) => url,
            Err(e) => {
                let _ = ready.send(Err(EngineError::Boot(e.to_string())));
                return;
            }
        };
        // Never loaded; MainWorker only needs a main-module identity.
        let main_module = root_url.join("__oj_engine_main__.mjs").unwrap();
        let mut worker = match worker::build_worker(&config, &main_module, module_host) {
            Ok(worker) => worker,
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };
        if let Some(hooks) = hooks {
            bridge::install(&mut worker, hooks);
        }

        let oom = Arc::new(AtomicBool::new(false));
        if config.memory_limit_bytes.is_some() {
            let handle = worker.js_runtime.v8_isolate().thread_safe_handle();
            let oom = oom.clone();
            worker
                .js_runtime
                .add_near_heap_limit_callback(move |current, _initial| {
                    oom.store(true, Ordering::SeqCst);
                    handle.terminate_execution();
                    // Raise the limit so V8 can unwind while the termination
                    // lands, instead of aborting the process.
                    current * 2
                });
        }
        let isolate_handle = worker.js_runtime.v8_isolate().thread_safe_handle();
        // The engine's one watchdog thread; every deadline-carrying job arms
        // it instead of spawning a thread of its own.
        let watchdog = Watchdog::spawn(isolate_handle.clone());

        if ready.send(Ok(isolate_handle.clone())).is_err() {
            return;
        }

        // The scheduler: call jobs run concurrently on the one isolate. A call
        // is set up exclusively (module load + evaluate + invoke, which itself
        // progresses the event loop), then parked as a pending promise; the
        // loop below interleaves accepting new jobs, polling every pending
        // promise, and driving the event loop, so a call that awaits a fetch
        // back into the caller (an SSR loader hitting its own dev server)
        // never deadlocks behind itself.
        enum Tick {
            Job(Job),
            Settled(
                usize,
                Result<v8::Global<v8::Value>, Box<deno_core::error::JsError>>,
            ),
            /// A parked call's deadline passed with its promise still pending:
            /// abandon the promise and fail that one call (see
            /// [`JsEngine::call_with_deadline`]).
            Expired,
            /// The event loop failed (an uncaught error) with calls still
            /// pending: nothing can settle them anymore.
            Broken(deno_core::error::CoreError),
            Closed,
        }
        let mut pending: Vec<PendingCall> = Vec::new();
        // The event loop drained completely (no ops, no live timers): only a
        // new job can create work, so skip event-loop polling until one
        // arrives and the scheduler parks instead of spinning. While the loop
        // HAS work it is polled even with no call pending — a module may have
        // left long-lived background work behind (the plugin host's
        // configureServer middleware server, a Miniflare instance), and that
        // work must keep serving between hook calls.
        let mut event_loop_idle = false;
        let mut eval_counter: u64 = 0;
        loop {
            // The earliest parked-call deadline, re-derived per iteration: the
            // pending set only changes between iterations.
            let next_deadline = pending.iter().filter_map(|p| p.deadline).min();
            let mut expiry = next_deadline.map(|d| Box::pin(tokio::time::sleep_until(d)));
            let tick = std::future::poll_fn(|cx| {
                match rx.poll_recv(cx) {
                    std::task::Poll::Ready(Some(job)) => {
                        return std::task::Poll::Ready(Tick::Job(job))
                    }
                    std::task::Poll::Ready(None) => return std::task::Poll::Ready(Tick::Closed),
                    std::task::Poll::Pending => {}
                }
                for (i, p) in pending.iter_mut().enumerate() {
                    if let std::task::Poll::Ready(r) = p.fut.as_mut().poll(cx) {
                        return std::task::Poll::Ready(Tick::Settled(i, r));
                    }
                }
                if let Some(expiry) = expiry.as_mut() {
                    if expiry.as_mut().poll(cx).is_ready() {
                        return std::task::Poll::Ready(Tick::Expired);
                    }
                }
                if !event_loop_idle {
                    match worker
                        .js_runtime
                        .poll_event_loop(cx, PollEventLoopOptions::default())
                    {
                        std::task::Poll::Ready(Ok(())) => event_loop_idle = true,
                        std::task::Poll::Ready(Err(e)) => {
                            return std::task::Poll::Ready(Tick::Broken(e))
                        }
                        std::task::Poll::Pending => {}
                    }
                }
                std::task::Poll::Pending
            })
            .await;
            if matches!(tick, Tick::Job(_)) {
                event_loop_idle = false;
            }
            match tick {
                Tick::Closed => {
                    // The tick polls the job channel before the event loop, so
                    // an engine dropped right after its last call closes here
                    // with V8's freshly produced code-cache blobs still parked
                    // as pending `code_cache_ready` callbacks. One noop-waker
                    // poll invokes them (the loader writes synchronously, see
                    // deno_core's ExtCodeCache note) and cannot hang on
                    // long-lived background ops the way running the loop to
                    // completion would. The poll still runs due timers and
                    // promise continuations, JS the drop path never executed
                    // before, so the watchdog bounds it: `JsEngine::drop`
                    // joins this thread, and an unbounded continuation here
                    // would wedge the dropping thread with it.
                    if !event_loop_idle {
                        let guard = DeadlineGuard::arm(&watchdog, Duration::from_millis(250));
                        let noop = std::task::Waker::noop();
                        let mut cx = std::task::Context::from_waker(noop);
                        let _ = worker
                            .js_runtime
                            .poll_event_loop(&mut cx, PollEventLoopOptions::default());
                        let _ = guard.disarm();
                    }
                    break;
                }
                Tick::Job(Job::Eval {
                    input,
                    deadline,
                    reply,
                }) => {
                    eval_counter += 1;
                    let guard = deadline.map(|d| DeadlineGuard::arm(&watchdog, d));
                    let result =
                        run_eval(&mut worker, &config, &root_url, eval_counter, input).await;
                    let result = classify(&mut worker, result, guard, &oom);
                    let _ = reply.send(result);
                }
                Tick::Job(Job::Call {
                    module,
                    export,
                    args,
                    deadline,
                    reply,
                }) => {
                    // The watchdog stays armed for the call's whole life. In
                    // the EXCLUSIVE setup phase (module evaluation + the
                    // synchronous part of the call) no other JS can be on the
                    // stack, so a termination can never hit an innocent job.
                    // While the call is parked the scheduler's expiry tick
                    // normally handles the deadline first and disarms the
                    // watchdog untouched; the watchdog only matters when a
                    // parked call's continuation wedges the event loop in a
                    // busy loop — then the expiry tick can never run and the
                    // termination is the only way the isolate comes back.
                    let deadline_at = deadline.map(|d| tokio::time::Instant::now() + d);
                    let guard = deadline.map(|d| DeadlineGuard::arm(&watchdog, d));
                    match setup_call(&mut worker, &config, &module, &export, args).await {
                        Ok(fut) => {
                            if guard.as_ref().is_some_and(DeadlineGuard::fired) {
                                // Setup outlived the deadline but completed
                                // anyway (the termination raced completion):
                                // the call is expired, not broken.
                                let _ =
                                    classify(&mut worker, Ok(serde_json::Value::Null), guard, &oom);
                                let _ = reply.send(Err(EngineError::Deadline));
                            } else {
                                pending.push(PendingCall {
                                    fut: Box::pin(fut),
                                    deadline: deadline_at,
                                    guard,
                                    reply,
                                });
                            }
                        }
                        Err(e) => {
                            let result = classify(&mut worker, Err(e), guard, &oom);
                            let _ = reply.send(result);
                        }
                    }
                }
                Tick::Settled(i, result) => {
                    let call = pending.swap_remove(i);
                    let result = settled_to_json(&mut worker, result);
                    let result = classify(&mut worker, result, call.guard, &oom);
                    let _ = call.reply.send(result);
                }
                Tick::Expired => {
                    // Every parked call at or past its deadline fails now; the
                    // dropped future abandons the promise, the isolate and the
                    // other calls carry on. Disarming through `classify`
                    // cancels a termination the watchdog got in first.
                    let now = tokio::time::Instant::now();
                    let mut i = 0;
                    while i < pending.len() {
                        if pending[i].deadline.is_some_and(|d| d <= now) {
                            let call = pending.swap_remove(i);
                            let _ = classify(
                                &mut worker,
                                Ok(serde_json::Value::Null),
                                call.guard,
                                &oom,
                            );
                            let _ = call.reply.send(Err(EngineError::Deadline));
                        } else {
                            i += 1;
                        }
                    }
                }
                Tick::Broken(e) => {
                    // An uncaught error broke the event loop (the process
                    // would die under Node): deliver promises that settled on
                    // the final turn, fail the rest with the error. With
                    // nothing pending the error would otherwise vanish: say it.
                    let error = e.to_string();
                    if pending.is_empty() {
                        eprintln!("oj_js: uncaught error on the engine event loop: {error}");
                    }
                    let noop = std::task::Waker::noop();
                    let mut cx = std::task::Context::from_waker(noop);
                    for mut call in std::mem::take(&mut pending) {
                        let result = match call.fut.as_mut().poll(&mut cx) {
                            std::task::Poll::Ready(r) => settled_to_json(&mut worker, r),
                            std::task::Poll::Pending => Err(EngineError::Js(error.clone())),
                        };
                        // A call whose watchdog terminated the wedged loop is
                        // the one that expired; classify maps it to Deadline
                        // and un-poisons the isolate for the survivors.
                        let result = classify(&mut worker, result, call.guard, &oom);
                        let _ = call.reply.send(result);
                    }
                }
            }
        }
    });
}

/// The result of a call's promise, as `call_with_args` settles it.
type SettledResult = Result<v8::Global<v8::Value>, Box<deno_core::error::JsError>>;

/// A call whose module ran and whose function was invoked, waiting for the
/// returned promise to settle while the scheduler drives the event loop.
/// `deadline` is when the scheduler abandons the still-pending promise and
/// fails the call with [`EngineError::Deadline`]; `guard` is the same
/// deadline's watchdog, kept armed so a continuation that wedges the event
/// loop in a busy loop is terminated (the responsive path disarms it first).
struct PendingCall {
    fut: std::pin::Pin<Box<dyn std::future::Future<Output = SettledResult>>>,
    deadline: Option<tokio::time::Instant>,
    guard: Option<DeadlineGuard>,
    reply: Reply,
}

fn settled_to_json(
    worker: &mut MainWorker,
    result: SettledResult,
) -> Result<serde_json::Value, EngineError> {
    match result {
        Ok(global) => {
            deno_core::scope!(scope, &mut worker.js_runtime);
            let local = v8::Local::new(scope, global);
            Ok(v8_to_json(scope, local))
        }
        Err(e) => Err(EngineError::Js(e.to_string())),
    }
}

/// Maps a job result onto limit errors: an isolate termination caused by the
/// heap-limit callback becomes `MemoryLimit`, one caused by a deadline
/// watchdog becomes `Deadline`. Also un-poisons the isolate so later jobs run.
fn classify(
    worker: &mut MainWorker,
    result: Result<serde_json::Value, EngineError>,
    guard: Option<DeadlineGuard>,
    oom: &AtomicBool,
) -> Result<serde_json::Value, EngineError> {
    let deadline_fired = guard.map(|g| g.disarm()).unwrap_or(false);
    let oom_fired = oom.swap(false, Ordering::SeqCst);
    if deadline_fired || oom_fired {
        worker.js_runtime.v8_isolate().cancel_terminate_execution();
    }
    match result {
        Err(_) if oom_fired => Err(EngineError::MemoryLimit),
        Err(_) if deadline_fired => Err(EngineError::Deadline),
        other => other,
    }
}

/// One long-lived watchdog thread per engine, terminating JS execution when
/// an armed deadline passes. Every deadline-carrying job used to spawn (and
/// join) its own OS thread — one per plugin hook call and CSS compile; the
/// engine now owns a single thread that all [`DeadlineGuard`]s arm and disarm
/// through shared state. Firing happens WITH the state lock held, and disarm
/// takes the same lock, so the old mutex-synchronized protocol is preserved:
/// a firing watchdog can never poison the job that comes after the one it was
/// armed for, and a disarm's `fired` answer is settled.
///
/// A dedicated thread rather than a tokio task on purpose: the ScriptEngine
/// and CSS spawn paths do not reliably carry a runtime handle, and the
/// watchdog must keep ticking while the engine thread itself is wedged in
/// synchronous JS.
struct Watchdog {
    shared: Arc<WatchdogShared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

struct WatchdogShared {
    state: Mutex<WatchdogState>,
    cv: std::sync::Condvar,
}

struct WatchdogState {
    next_id: u64,
    /// Armed deadlines by guard id; several calls park concurrently, each
    /// with its own deadline.
    armed: std::collections::HashMap<u64, ArmedDeadline>,
    shutdown: bool,
}

struct ArmedDeadline {
    at: std::time::Instant,
    fired: Arc<AtomicBool>,
}

impl Watchdog {
    fn spawn(handle: v8::IsolateHandle) -> Watchdog {
        let shared = Arc::new(WatchdogShared {
            state: Mutex::new(WatchdogState {
                next_id: 0,
                armed: std::collections::HashMap::new(),
                shutdown: false,
            }),
            cv: std::sync::Condvar::new(),
        });
        let thread = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("oj-js-watchdog".into())
                .spawn(move || watchdog_thread(&shared, &handle))
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
        {
            let mut state = self
                .shared
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.shutdown = true;
        }
        self.shared.cv.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn watchdog_thread(shared: &WatchdogShared, handle: &v8::IsolateHandle) {
    let mut state = shared
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    loop {
        if state.shutdown {
            return;
        }
        let now = std::time::Instant::now();
        // Fire every armed deadline that has passed — under the lock, so a
        // concurrent disarm reads a settled verdict — and drop it from the
        // set (its termination is done; the guard still owns the flag).
        let mut due = false;
        state.armed.retain(|_, armed| {
            if armed.at <= now {
                armed.fired.store(true, Ordering::SeqCst);
                due = true;
                false
            } else {
                true
            }
        });
        if due {
            handle.terminate_execution();
        }
        state = match state.armed.values().map(|a| a.at).min() {
            Some(next) => {
                let (state, _) = shared
                    .cv
                    .wait_timeout(state, next.saturating_duration_since(now))
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state
            }
            None => shared
                .cv
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        };
    }
}

/// One job's handle on the engine's [`Watchdog`]: armed at submit, disarmed
/// through `classify` when the job settles.
struct DeadlineGuard {
    id: u64,
    fired: Arc<AtomicBool>,
    shared: Arc<WatchdogShared>,
}

impl DeadlineGuard {
    fn arm(watchdog: &Watchdog, deadline: Duration) -> Self {
        let fired = Arc::new(AtomicBool::new(false));
        let id = {
            let mut state = watchdog
                .shared
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let id = state.next_id;
            state.next_id += 1;
            state.armed.insert(
                id,
                ArmedDeadline {
                    at: std::time::Instant::now() + deadline,
                    fired: fired.clone(),
                },
            );
            id
        };
        // A new earliest deadline: wake the thread to re-derive its wait.
        watchdog.shared.cv.notify_all();
        DeadlineGuard {
            id,
            fired,
            shared: Arc::clone(&watchdog.shared),
        }
    }

    /// Disarms the watchdog for this job and reports whether it fired.
    fn disarm(self) -> bool {
        // Taking the state lock waits out a watchdog that is mid-fire (it
        // fires holding the lock), so `fired` is settled once we read it.
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.armed.remove(&self.id);
        drop(state);
        self.fired.load(Ordering::SeqCst)
    }

    /// Whether the watchdog fired, without disarming it.
    fn fired(&self) -> bool {
        // The lock waits out a watchdog mid-fire, so the read is settled.
        let _state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.fired.load(Ordering::SeqCst)
    }
}

fn js_err(e: impl std::fmt::Display) -> EngineError {
    EngineError::Js(e.to_string())
}

// JSON crosses the Rust/V8 boundary through V8's own JSON parser rather than
// serde_v8: serializing serde_json::Value with serde_v8 breaks when any crate
// in the build enables serde_json's "arbitrary_precision" feature (numbers
// then serialize as an internal struct, which V8 sees as an object).

fn json_to_v8<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: &serde_json::Value,
) -> Result<v8::Local<'s, v8::Value>, EngineError> {
    let text = serde_json::to_string(value).map_err(js_err)?;
    let text = v8::String::new(scope, &text)
        .ok_or_else(|| EngineError::Js("argument is too large for V8".into()))?;
    v8::json::parse(scope, text).ok_or_else(|| EngineError::Js("argument is not valid JSON".into()))
}

/// `null` when the value has no JSON representation (functions, undefined,
/// cycles).
fn v8_to_json(scope: &mut v8::PinScope<'_, '_>, value: v8::Local<v8::Value>) -> serde_json::Value {
    v8::json::stringify(scope, value)
        .map(|text| text.to_rust_string_lossy(scope))
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(serde_json::Value::Null)
}

fn resolve_module_path(config: &EngineConfig, path: &std::path::Path) -> Result<Url, EngineError> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        config.root.join(path)
    };
    deno_path_util::url_from_file_path(&abs).map_err(js_err)
}

/// A call's module: an absolute URL is used as-is (version-stamped host
/// specifiers, virtual-module schemes), anything else is a filesystem path.
/// Single letters before `:` are not schemes here, so Windows drive paths
/// stay paths.
fn resolve_module_spec(config: &EngineConfig, spec: &str) -> Result<Url, EngineError> {
    if let Ok(url) = Url::parse(spec) {
        if url.scheme().len() > 1 {
            return Ok(url);
        }
    }
    resolve_module_path(config, std::path::Path::new(spec))
}

async fn run_eval(
    worker: &mut MainWorker,
    config: &EngineConfig,
    root_url: &Url,
    counter: u64,
    input: EvalInput,
) -> Result<serde_json::Value, EngineError> {
    let id = match input {
        EvalInput::Source(source) => {
            // Unique synthetic specifier per eval: the module map is
            // per-isolate and persistent, so specifiers must not collide.
            let url = root_url
                .join(&format!("__oj_eval_{counter}__.mjs"))
                .map_err(js_err)?;
            worker
                .js_runtime
                .load_side_es_module_from_code(&url, source)
                .await
                .map_err(js_err)?
        }
        EvalInput::Path(path) => {
            let url = resolve_module_path(config, &path)?;
            worker.preload_side_module(&url).await.map_err(js_err)?
        }
    };
    worker.evaluate_module(id).await.map_err(js_err)?;
    worker.run_event_loop(false).await.map_err(js_err)?;

    let ns = worker.js_runtime.get_module_namespace(id).map_err(js_err)?;
    deno_core::scope!(scope, &mut worker.js_runtime);
    let ns = v8::Local::new(scope, ns);
    let key = v8::String::new(scope, "default").unwrap();
    let value = ns.get(scope, key.into());
    Ok(value
        .map(|v| v8_to_json(scope, v))
        .unwrap_or(serde_json::Value::Null))
}

/// Loads and evaluates the module, invokes the export, and returns the future
/// of its settled result. The future is independent of the worker borrow, so
/// the scheduler polls it alongside the event loop and other pending calls.
async fn setup_call(
    worker: &mut MainWorker,
    config: &EngineConfig,
    module: &str,
    export: &str,
    args: Vec<serde_json::Value>,
) -> Result<
    impl std::future::Future<Output = Result<v8::Global<v8::Value>, Box<deno_core::error::JsError>>>
        + use<>,
    EngineError,
> {
    let url = resolve_module_spec(config, module)?;
    let id = worker.preload_side_module(&url).await.map_err(js_err)?;
    worker.evaluate_module(id).await.map_err(js_err)?;

    let ns = worker.js_runtime.get_module_namespace(id).map_err(js_err)?;
    let (function, arg_globals) = {
        deno_core::scope!(scope, &mut worker.js_runtime);
        let ns = v8::Local::new(scope, ns);
        let key = v8::String::new(scope, export)
            .ok_or_else(|| EngineError::Js(format!("invalid export name \"{export}\"")))?;
        let value = ns
            .get(scope, key.into())
            .ok_or_else(|| EngineError::Js(format!("module {url} has no export \"{export}\"")))?;
        let function: v8::Local<v8::Function> = value.try_into().map_err(|_| {
            EngineError::Js(format!("export \"{export}\" of {url} is not a function"))
        })?;
        let function = v8::Global::new(scope, function);
        let mut arg_globals = Vec::with_capacity(args.len());
        for arg in &args {
            let local = json_to_v8(scope, arg)?;
            arg_globals.push(v8::Global::new(scope, local));
        }
        (function, arg_globals)
    };

    Ok(worker.js_runtime.call_with_args(&function, &arg_globals))
}
