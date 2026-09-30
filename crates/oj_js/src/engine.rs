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
    /// Spawns an engine. `module_host` governs module loading before the
    /// engine's own byonm loader (see [`ModuleHost::channel`]); `hooks`
    /// installs JS→Rust bridge globals before any module runs (see
    /// [`EngineHooks`]).
    pub fn spawn(
        config: EngineConfig,
        module_host: Option<ModuleHost>,
        hooks: Option<EngineHooks>,
    ) -> Result<JsEngine, EngineError> {
        init_v8_platform_once();
        let default_deadline = config.default_deadline;
        // Unbounded on purpose: every caller awaits its reply before it can
        // send again, so queue depth is bounded by caller concurrency; a
        // capacity would only add an artificial stall. The scheduler polls
        // the receiver inside its tick, which rules out std::sync::mpsc.
        let (tx, rx) = mpsc::unbounded_channel();
        if let Some(registry) = &config.registry {
            registry.register(&tx);
        }
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

    /// Executes an ES module to completion. `deadline` bounds this one job;
    /// `None` falls back to the engine's default deadline.
    pub async fn eval(
        &self,
        input: EvalInput,
        deadline: Option<Duration>,
    ) -> Result<serde_json::Value, EngineError> {
        let deadline = deadline.or(self.default_deadline);
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
    /// `deadline` bounds this one call (`None` falls back to the engine's
    /// default): through the exclusive setup phase a watchdog terminates a
    /// wedged run; once the call parks, the scheduler abandons the
    /// still-pending promise at the deadline, replying
    /// [`EngineError::Deadline`] while the isolate and every concurrent call
    /// carry on. The watchdog stays armed underneath solely for a
    /// continuation that wedges the event loop in a busy loop.
    pub async fn call(
        &self,
        module: impl Into<String>,
        export: &str,
        args: Vec<serde_json::Value>,
        deadline: Option<Duration>,
    ) -> Result<serde_json::Value, EngineError> {
        let module = module.into();
        let export = export.to_string();
        let deadline = deadline.or(self.default_deadline);
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

/// The engines a memory probe can fan a GC over. A cloneable handle the
/// embedder owns (oj: one per dev server, behind its debug endpoint); engines
/// join through [`crate::EngineConfig::registry`], so respawns and revives
/// re-register themselves without the owner keeping a list.
///
/// Senders are held WEAK: an engine thread exits when its job channel closes
/// and `JsEngine::drop` JOINS that thread, so a strong sender here would
/// deadlock every drop. A plain mutexed Vec on purpose — a handful of
/// entries, touched at spawn and probe time only — pruned on register so
/// respawn churn cannot grow it past the live set.
#[derive(Clone, Default)]
pub struct EngineRegistry {
    engines: Arc<Mutex<Vec<mpsc::WeakUnboundedSender<Job>>>>,
}

impl std::fmt::Debug for EngineRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "EngineRegistry({} slots)", lock(&self.engines).len())
    }
}

impl EngineRegistry {
    pub fn new() -> EngineRegistry {
        EngineRegistry::default()
    }

    fn register(&self, tx: &mpsc::UnboundedSender<Job>) {
        let mut engines = lock(&self.engines);
        engines.retain(|weak| weak.upgrade().is_some_and(|tx| !tx.is_closed()));
        engines.push(tx.downgrade());
    }

    /// Force a full V8 collection in every registered live engine and return
    /// how many acknowledged having RUN it within `wait` (shared across
    /// engines). Blocking: async callers go through spawn_blocking.
    pub fn collect_garbage(&self, wait: Duration) -> usize {
        let senders: Vec<mpsc::WeakUnboundedSender<Job>> = lock(&self.engines).clone();
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
        collected
    }
}

fn init_v8_platform_once() {
    static V8_INIT: Once = Once::new();
    // Exactly once process-wide, before the first isolate on any thread.
    V8_INIT.call_once(|| deno_core::JsRuntime::init_platform(None));
}
