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

mod host;
mod loader;
mod worker;

pub use host::HostFuture;
pub use host::HostModule;
pub use host::HostModuleType;
pub use host::HostResolved;
pub use host::ModuleHost;

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
}

impl EngineConfig {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            memory_limit_bytes: None,
            default_deadline: None,
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
/// (the job channel closes, the loop ends, the thread is joined).
pub struct JsEngine {
    tx: Option<mpsc::UnboundedSender<Job>>,
    thread: Option<std::thread::JoinHandle<()>>,
    default_deadline: Option<Duration>,
}

impl JsEngine {
    pub fn spawn(config: EngineConfig) -> Result<JsEngine, EngineError> {
        Self::spawn_inner(config, None)
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
        Self::spawn_inner(config, Some(host::HostBridge::new(runtime, module_host)))
    }

    fn spawn_inner(
        config: EngineConfig,
        module_host: Option<host::HostBridge>,
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
            .spawn(move || engine_thread(config, module_host, rx, ready_tx))
            .map_err(|e| EngineError::Boot(e.to_string()))?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(JsEngine {
                tx: Some(tx),
                thread: Some(thread),
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
        let module = module.into();
        let export = export.to_string();
        let deadline = self.default_deadline;
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
        self.tx
            .as_ref()
            .ok_or(EngineError::Closed)?
            .send(make_job(reply_tx))
            .map_err(|_| EngineError::Closed)?;
        reply_rx.await.map_err(|_| EngineError::Closed)?
    }
}

impl Drop for JsEngine {
    fn drop(&mut self) {
        // Close the channel first so the engine thread's recv loop ends.
        self.tx.take();
        if let Some(thread) = self.thread.take() {
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
    mut rx: mpsc::UnboundedReceiver<Job>,
    ready: std::sync::mpsc::Sender<Result<(), EngineError>>,
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

        if ready.send(Ok(())).is_err() {
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
            /// The event loop failed (an uncaught error) with calls still
            /// pending: nothing can settle them anymore.
            Broken(deno_core::error::CoreError),
            /// A pending call outlived its deadline. Reached when the hang is
            /// a never-settling promise: the event loop drains, the scheduler
            /// parks, and `terminate_execution` has nothing to terminate — so
            /// the park itself is bounded by the earliest pending deadline.
            Expired(usize),
            Closed,
        }
        let mut pending: Vec<PendingCall> = Vec::new();
        // The event loop drained while calls were still pending: their
        // promises can only settle through future jobs (or never — a hung
        // request, as under Node). Skip event-loop polling until new work
        // arrives, so the scheduler parks instead of spinning.
        let mut event_loop_idle = false;
        let mut eval_counter: u64 = 0;
        loop {
            // A no-deadline call may park indefinitely (SSR semantics); one
            // with a deadline bounds the park so it can still be classified.
            let earliest = pending.iter().filter_map(|p| p.expires_at).min();
            let mut timer = earliest.map(|at| Box::pin(tokio::time::sleep_until(at)));
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
                if !pending.is_empty() && !event_loop_idle {
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
                if let Some(timer) = timer.as_mut() {
                    if std::future::Future::poll(timer.as_mut(), cx).is_ready() {
                        let now = tokio::time::Instant::now();
                        if let Some(i) = pending
                            .iter()
                            .position(|p| p.expires_at.is_some_and(|at| at <= now))
                        {
                            return std::task::Poll::Ready(Tick::Expired(i));
                        }
                    }
                }
                std::task::Poll::Pending
            })
            .await;
            if matches!(tick, Tick::Job(_)) {
                event_loop_idle = false;
            }
            match tick {
                Tick::Closed => break,
                Tick::Job(Job::Eval {
                    input,
                    deadline,
                    reply,
                }) => {
                    eval_counter += 1;
                    let guard = deadline.map(|d| DeadlineGuard::arm(isolate_handle.clone(), d));
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
                    let guard = deadline.map(|d| DeadlineGuard::arm(isolate_handle.clone(), d));
                    let expires_at = deadline.map(|d| tokio::time::Instant::now() + d);
                    match setup_call(&mut worker, &config, &module, &export, args).await {
                        Ok(fut) => pending.push(PendingCall {
                            fut: Box::pin(fut),
                            guard,
                            expires_at,
                            reply,
                        }),
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
                Tick::Expired(i) => {
                    // Drop the settle future: nothing will resolve it. The
                    // module instance leaks with the isolate, which survives.
                    let call = pending.swap_remove(i);
                    let _ = classify(&mut worker, Ok(serde_json::Value::Null), call.guard, &oom);
                    let _ = call.reply.send(Err(EngineError::Deadline));
                }
                Tick::Broken(e) => {
                    // An uncaught error broke the event loop (the process
                    // would die under Node): deliver promises that settled on
                    // the final turn, fail the rest with the error.
                    let error = e.to_string();
                    let noop = std::task::Waker::noop();
                    let mut cx = std::task::Context::from_waker(noop);
                    for mut call in std::mem::take(&mut pending) {
                        let result = match call.fut.as_mut().poll(&mut cx) {
                            std::task::Poll::Ready(r) => settled_to_json(&mut worker, r),
                            std::task::Poll::Pending => Err(EngineError::Js(error.clone())),
                        };
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
struct PendingCall {
    fut: std::pin::Pin<Box<dyn std::future::Future<Output = SettledResult>>>,
    guard: Option<DeadlineGuard>,
    /// When the scheduler itself times the call out (`Tick::Expired`), for the
    /// hang shape the watchdog's `terminate_execution` cannot reach: a parked,
    /// never-settling promise on an idle isolate.
    expires_at: Option<tokio::time::Instant>,
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

/// Terminates JS execution from a helper thread once the deadline passes.
/// Arming and disarming synchronize on a mutex so a firing watchdog can never
/// poison the job that comes after the one it was armed for.
struct DeadlineGuard {
    disarmed: Arc<Mutex<bool>>,
    fired: Arc<AtomicBool>,
    cancel_tx: std::sync::mpsc::Sender<()>,
}

impl DeadlineGuard {
    fn arm(handle: v8::IsolateHandle, deadline: Duration) -> Self {
        let disarmed = Arc::new(Mutex::new(false));
        let fired = Arc::new(AtomicBool::new(false));
        let (cancel_tx, cancel_rx) = std::sync::mpsc::channel::<()>();
        {
            let disarmed = disarmed.clone();
            let fired = fired.clone();
            std::thread::spawn(move || {
                if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) =
                    cancel_rx.recv_timeout(deadline)
                {
                    let disarmed = disarmed.lock().unwrap();
                    if !*disarmed {
                        fired.store(true, Ordering::SeqCst);
                        handle.terminate_execution();
                    }
                }
            });
        }
        DeadlineGuard {
            disarmed,
            fired,
            cancel_tx,
        }
    }

    /// Disarms the watchdog and reports whether it fired.
    fn disarm(self) -> bool {
        // Taking the lock waits out a watchdog that is mid-fire, so `fired`
        // is settled once we read it.
        *self.disarmed.lock().unwrap() = true;
        let _ = self.cancel_tx.send(());
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
