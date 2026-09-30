// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The [`JsEngine`] handle: `Send + Sync`, submits jobs over a channel to the
//! engine thread and awaits the reply.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Once;
use std::time::Duration;

use deno_core::v8;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

use crate::bridge::EngineHooks;
use crate::host;
use crate::host::ModuleHost;
use crate::lock;
use crate::scheduler;
use crate::EngineConfig;
use crate::EngineError;
use crate::EvalInput;

pub(crate) type Reply = oneshot::Sender<Result<serde_json::Value, EngineError>>;

pub(crate) enum Job {
    /// Execute an ES module to completion (event loop drained). Replies with
    /// the module's default export when it is JSON-serializable, else `null`.
    Eval {
        input: EvalInput,
        deadline: Option<Duration>,
        reply: Reply,
    },
    /// Force a full V8 collection and acknowledge when it has RUN — a
    /// completion barrier. A job, not an isolate interrupt: interrupts only
    /// fire while JS executes, so an idle engine — the exact state a memory
    /// probe measures — would defer the collection past the measurement.
    Gc { reply: std::sync::mpsc::Sender<()> },
    /// Execute a module, then call one of its exports with JSON arguments.
    /// A returned promise is resolved before replying; calls run concurrently
    /// on the one isolate.
    Call {
        module: String,
        export: String,
        args: Vec<serde_json::Value>,
        deadline: Option<Duration>,
        reply: Reply,
    },
}

impl Job {
    /// The reply for a job reaching a condemned isolate (its heap cap fired):
    /// fail fast until the owner replaces the engine. A GC barrier still acks.
    pub(crate) fn refuse_condemned(self) {
        match self {
            Job::Eval { reply, .. } | Job::Call { reply, .. } => {
                let _ = reply.send(Err(EngineError::MemoryLimit));
            }
            Job::Gc { reply } => {
                let _ = reply.send(());
            }
        }
    }
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
        Self::spawn_inner(
            config,
            Some(host::HostBridge::new(runtime, module_host)),
            None,
        )
    }

    /// Spawns an engine with JS→Rust bridges installed as globals before any
    /// module runs (see [`EngineHooks`]).
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
        ENGINES.lock().unwrap().push(tx.downgrade());
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("oj-js-engine".into())
            // V8 + deeply recursive module instantiation want more than the
            // 2MB default, especially in debug builds.
            .stack_size(8 * 1024 * 1024)
            .spawn(move || scheduler::engine_thread(config, module_host, hooks, rx, ready_tx))
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

    /// Force a full garbage collection in this engine and return once it has
    /// run (false when the engine is already gone). A memory-probing
    /// instrument, never a runtime lever: V8 schedules its own collections
    /// better than callers can. Blocking; async callers go through
    /// spawn_blocking.
    pub fn collect_garbage(&self, wait: Duration) -> bool {
        let (ack_tx, ack_rx) = std::sync::mpsc::channel();
        let sent = match self.tx.lock().unwrap().as_ref() {
            Some(tx) => tx.send(Job::Gc { reply: ack_tx }).is_ok(),
            None => false,
        };
        sent && ack_rx.recv_timeout(wait).is_ok()
    }

    /// Gives up on the engine without waiting for it: terminates whatever JS
    /// is running, closes the job channel, and detaches the thread so no
    /// caller ever joins it. A job wedged in NATIVE code (a napi call, a
    /// blocking child wait) cannot be interrupted: that thread and its isolate
    /// leak until the block ends — the accepted cost of abandoning in-process
    /// what a process kill used to reclaim.
    pub fn abandon(&self) {
        self.isolate.terminate_execution();
        lock(&self.tx).take();
        // Dropping the JoinHandle detaches the thread; Drop will find None.
        lock(&self.thread).take();
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
    /// Through the call's exclusive setup phase a watchdog terminates a
    /// wedged run. Once the call parks on its returned promise the scheduler
    /// abandons the still-pending promise at the deadline, replying
    /// [`EngineError::Deadline`] while the isolate and every concurrent call
    /// carry on; the watchdog stays armed underneath solely for a
    /// continuation that wedges the event loop in a busy loop.
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
        lock(&self.tx)
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
        lock(&self.tx).take();
        let thread = lock(&self.thread).take();
        if let Some(thread) = thread {
            let _ = thread.join();
        }
    }
}

/// Every live engine's job sender, registered WEAK at spawn: the process-wide
/// fan-out for the memory-probing GC reaches every engine, including ones
/// added later, without a hand-enumerated field list. Weak is load-bearing:
/// an engine thread exits when its job channel closes and `JsEngine::drop`
/// JOINS that thread, so a strong sender here would deadlock every drop.
static ENGINES: Mutex<Vec<mpsc::WeakUnboundedSender<Job>>> = Mutex::new(Vec::new());

/// Force a full V8 collection in every live engine and return how many
/// acknowledged having RUN it within `wait` (shared across engines). Blocking:
/// async callers go through spawn_blocking.
pub fn collect_all_garbage(wait: Duration) -> usize {
    let senders: Vec<mpsc::WeakUnboundedSender<Job>> = ENGINES.lock().unwrap().clone();
    let mut acks = Vec::new();
    for weak in &senders {
        let Some(tx) = weak.upgrade() else { continue };
        let (ack_tx, ack_rx) = std::sync::mpsc::channel();
        if tx.send(Job::Gc { reply: ack_tx }).is_ok() {
            acks.push(ack_rx);
        }
    }
    let deadline = std::time::Instant::now() + wait;
    let mut collected = 0usize;
    for rx in acks {
        let left = deadline
            .saturating_duration_since(std::time::Instant::now())
            .max(Duration::from_millis(1));
        if rx.recv_timeout(left).is_ok() {
            collected += 1;
        }
    }
    ENGINES
        .lock()
        .unwrap()
        .retain(|weak| weak.upgrade().is_some_and(|tx| !tx.is_closed()));
    collected
}

fn init_v8_platform_once() {
    static V8_INIT: Once = Once::new();
    // Exactly once process-wide, before the first isolate on any thread.
    V8_INIT.call_once(|| deno_core::JsRuntime::init_platform(None));
}
