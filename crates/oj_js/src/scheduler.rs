// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The engine thread: call jobs run concurrently on the one isolate. A call
//! is set up exclusively (module load + evaluate + invoke), then parked as a
//! pending promise; the loop interleaves accepting new jobs, polling every
//! pending promise, and driving the event loop, so a call that awaits a fetch
//! back into the caller (an SSR loader hitting its own dev server) never
//! deadlocks behind itself.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use std::time::Duration;

use deno_core::url::Url;
use deno_core::v8;
use deno_core::PollEventLoopOptions;
use deno_runtime::worker::MainWorker;
use tokio::sync::mpsc;

use crate::bridge;
use crate::bridge::EngineHooks;
use crate::convert::js_err;
use crate::convert::json_to_v8;
use crate::convert::settled_to_json;
use crate::convert::v8_to_json;
use crate::convert::SettledResult;
use crate::engine::Job;
use crate::engine::Reply;
use crate::host::HostBridge;
use crate::watchdog::DeadlineGuard;
use crate::watchdog::Watchdog;
use crate::worker::build_worker;
use crate::EngineConfig;
use crate::EngineError;
use crate::EvalInput;

pub(crate) fn engine_thread(
    config: EngineConfig,
    module_host: Option<HostBridge>,
    hooks: Option<EngineHooks>,
    rx: mpsc::UnboundedReceiver<Job>,
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
        let mut scheduler = match Scheduler::boot(config, module_host, hooks) {
            Ok(scheduler) => scheduler,
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };
        if ready.send(Ok(scheduler.isolate_handle())).is_err() {
            return;
        }
        scheduler.run(rx).await;
    });
}

enum Tick {
    Job(Job),
    Settled(usize, SettledResult),
    /// A parked call's deadline passed with its promise still pending:
    /// abandon the promise and fail that one call.
    Expired,
    /// The event loop failed (an uncaught error) with calls still pending:
    /// nothing can settle them anymore.
    Broken(deno_core::error::CoreError),
    /// The near-heap-limit callback fired during event-loop progress. The
    /// termination can land mid-microtask and leave every parked promise
    /// permanently unsettled — no Broken, no Settled — so the flag is a tick
    /// of its own.
    MemoryExhausted,
    Closed,
}

/// A call whose module ran and whose function was invoked, waiting for the
/// returned promise to settle while the scheduler drives the event loop.
/// `guard` is the deadline's watchdog, kept armed so a continuation that
/// wedges the event loop in a busy loop is terminated.
struct PendingCall {
    fut: Pin<Box<dyn Future<Output = SettledResult>>>,
    deadline: Option<tokio::time::Instant>,
    guard: Option<DeadlineGuard>,
    reply: Reply,
}

/// What a finished job's termination state (watchdog, heap cap) says about
/// its outcome.
enum Verdict {
    Clean,
    Deadline,
    MemoryLimit,
}

impl Verdict {
    /// Maps a job error onto the termination that caused it.
    fn apply(
        self,
        result: Result<serde_json::Value, EngineError>,
    ) -> Result<serde_json::Value, EngineError> {
        match (self, result) {
            (Verdict::MemoryLimit, Err(_)) => Err(EngineError::MemoryLimit),
            (Verdict::Deadline, Err(_)) => Err(EngineError::Deadline),
            (_, result) => result,
        }
    }

    /// The error for a call cut off while parked (its promise abandoned).
    fn cutoff(self) -> EngineError {
        match self {
            Verdict::MemoryLimit => EngineError::MemoryLimit,
            _ => EngineError::Deadline,
        }
    }
}

struct Scheduler {
    worker: MainWorker,
    config: EngineConfig,
    root_url: Url,
    watchdog: Watchdog,
    /// Set by the near-heap-limit callback; the tick loop reads it right
    /// after each event-loop poll (the callback runs inside that poll).
    oom: Arc<AtomicBool>,
    pending: Vec<PendingCall>,
    /// The event loop drained completely (no ops, no live timers). Only tells
    /// the shutdown path whether a final flush is needed; the loop is polled
    /// on every wake regardless (see [`Scheduler::next_tick`]).
    event_loop_idle: bool,
    eval_counter: u64,
    /// The heap-limit callback fired: the isolate ran on and may have lost
    /// arbitrary state, so every later job fails fast as MemoryLimit until
    /// the owner (CSS revive, plugin-host respawn) replaces the engine.
    /// Without it, each background-work OOM permanently doubles the limit.
    condemned: bool,
}

impl Scheduler {
    fn boot(
        config: EngineConfig,
        module_host: Option<HostBridge>,
        hooks: Option<EngineHooks>,
    ) -> Result<Scheduler, EngineError> {
        let root_url = deno_path_util::url_from_directory_path(&config.root)
            .map_err(|e| EngineError::Boot(e.to_string()))?;
        // Never loaded; MainWorker only needs a main-module identity.
        let main_module = root_url.join("__oj_engine_main__.mjs").unwrap();
        let mut worker = build_worker(&config, &main_module, module_host)?;
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
        let watchdog = Watchdog::spawn(worker.js_runtime.v8_isolate().thread_safe_handle());

        Ok(Scheduler {
            worker,
            config,
            root_url,
            watchdog,
            oom,
            pending: Vec::new(),
            event_loop_idle: false,
            eval_counter: 0,
            condemned: false,
        })
    }

    fn isolate_handle(&mut self) -> v8::IsolateHandle {
        self.worker.js_runtime.v8_isolate().thread_safe_handle()
    }

    async fn run(&mut self, mut rx: mpsc::UnboundedReceiver<Job>) {
        loop {
            let tick = self.next_tick(&mut rx).await;
            if matches!(tick, Tick::Job(_)) {
                self.event_loop_idle = false;
            }
            match tick {
                Tick::Job(job) if self.condemned => job.refuse_condemned(),
                Tick::Job(Job::Eval {
                    input,
                    deadline,
                    reply,
                }) => self.eval_job(input, deadline, reply).await,
                Tick::Job(Job::Gc { reply }) => {
                    self.worker
                        .js_runtime
                        .v8_isolate()
                        .low_memory_notification();
                    let _ = reply.send(());
                }
                Tick::Job(Job::Call {
                    module,
                    export,
                    args,
                    deadline,
                    reply,
                }) => self.call_job(module, export, args, deadline, reply).await,
                Tick::Settled(i, result) => self.settled(i, result),
                Tick::Expired => self.expire(),
                Tick::MemoryExhausted => self.memory_exhausted(),
                Tick::Broken(e) => self.broken(e),
                Tick::Closed => {
                    self.flush_on_close();
                    break;
                }
            }
        }
    }

    async fn next_tick(&mut self, rx: &mut mpsc::UnboundedReceiver<Job>) -> Tick {
        // The earliest parked-call deadline; the pending set only changes
        // between iterations.
        let next_deadline = self.pending.iter().filter_map(|p| p.deadline).min();
        let mut expiry = next_deadline.map(|d| Box::pin(tokio::time::sleep_until(d)));
        std::future::poll_fn(|cx| {
            match rx.poll_recv(cx) {
                Poll::Ready(Some(job)) => return Poll::Ready(Tick::Job(job)),
                Poll::Ready(None) => return Poll::Ready(Tick::Closed),
                Poll::Pending => {}
            }
            for (i, p) in self.pending.iter_mut().enumerate() {
                if let Poll::Ready(r) = p.fut.as_mut().poll(cx) {
                    return Poll::Ready(Tick::Settled(i, r));
                }
            }
            if let Some(expiry) = expiry.as_mut() {
                if expiry.as_mut().poll(cx).is_ready() {
                    return Poll::Ready(Tick::Expired);
                }
            }
            // The event loop is polled even when drained: V8 posts its own
            // foreground work (the MemoryReducer's delayed GC tasks, which
            // return an idle heap's pages; incremental-marking steps) through
            // the platform, and only a poll drains that queue — skipping it
            // kept an idle isolate at its high-water heap. A drained poll
            // just registers the waker; nothing spins.
            match self
                .worker
                .js_runtime
                .poll_event_loop(cx, PollEventLoopOptions::default())
            {
                Poll::Ready(Ok(())) => self.event_loop_idle = true,
                Poll::Ready(Err(e)) => return Poll::Ready(Tick::Broken(e)),
                Poll::Pending => self.event_loop_idle = false,
            }
            // The heap-limit callback runs inside the poll above, so a
            // mid-pump exhaustion is visible right here — the tick never
            // depends on a waker the terminated JS can't fire.
            if self.oom.load(Ordering::SeqCst) {
                return Poll::Ready(Tick::MemoryExhausted);
            }
            Poll::Pending
        })
        .await
    }

    fn guard(&self, deadline: Option<Duration>) -> Option<DeadlineGuard> {
        deadline.map(|d| DeadlineGuard::arm(&self.watchdog, d))
    }

    /// Settles a finished job's guard against the oom flag, un-poisoning the
    /// isolate so later jobs run. A fired heap cap condemns the engine and
    /// sweeps every parked call (their promises can never settle).
    fn disarm(&mut self, guard: Option<DeadlineGuard>) -> Verdict {
        let deadline_fired = guard.map(DeadlineGuard::disarm).unwrap_or(false);
        let oom_fired = self.oom.swap(false, Ordering::SeqCst);
        if deadline_fired || oom_fired {
            self.worker
                .js_runtime
                .v8_isolate()
                .cancel_terminate_execution();
        }
        if oom_fired {
            self.condemn();
            Verdict::MemoryLimit
        } else if deadline_fired {
            Verdict::Deadline
        } else {
            Verdict::Clean
        }
    }

    /// Fails every parked call as MemoryLimit and cancels the heap-limit
    /// callback's termination — AFTER all guards are disarmed, so a watchdog
    /// firing mid-sweep cannot leave a termination pending for the next job.
    fn condemn(&mut self) {
        for call in std::mem::take(&mut self.pending) {
            if let Some(guard) = call.guard {
                let _ = guard.disarm();
            }
            let _ = call.reply.send(Err(EngineError::MemoryLimit));
        }
        self.worker
            .js_runtime
            .v8_isolate()
            .cancel_terminate_execution();
        self.condemned = true;
    }

    async fn eval_job(&mut self, input: EvalInput, deadline: Option<Duration>, reply: Reply) {
        self.eval_counter += 1;
        let counter = self.eval_counter;
        let guard = self.guard(deadline);
        let result = self.run_eval(counter, input).await;
        let verdict = self.disarm(guard);
        let _ = reply.send(verdict.apply(result));
    }

    async fn call_job(
        &mut self,
        module: String,
        export: String,
        args: Vec<serde_json::Value>,
        deadline: Option<Duration>,
        reply: Reply,
    ) {
        // The watchdog stays armed for the call's whole life. In the
        // EXCLUSIVE setup phase no other JS can be on the stack, so a
        // termination can never hit an innocent job. While the call is parked
        // the expiry tick normally handles the deadline; the watchdog only
        // matters when a continuation wedges the event loop in a busy loop.
        let deadline_at = deadline.map(|d| tokio::time::Instant::now() + d);
        let guard = self.guard(deadline);
        match self.setup_call(&module, &export, args).await {
            Ok(_) if guard.as_ref().is_some_and(DeadlineGuard::fired) => {
                // Setup outlived the deadline but completed anyway (the
                // termination raced completion): expired, not broken.
                let verdict = self.disarm(guard);
                let _ = reply.send(Err(verdict.cutoff()));
            }
            Ok(fut) => self.pending.push(PendingCall {
                fut: Box::pin(fut),
                deadline: deadline_at,
                guard,
                reply,
            }),
            Err(e) => {
                let verdict = self.disarm(guard);
                let _ = reply.send(verdict.apply(Err(e)));
            }
        }
    }

    fn settled(&mut self, i: usize, result: SettledResult) {
        let call = self.pending.swap_remove(i);
        let result = settled_to_json(&mut self.worker, result);
        let verdict = self.disarm(call.guard);
        let _ = call.reply.send(verdict.apply(result));
    }

    /// Fails every parked call at or past its deadline; the dropped future
    /// abandons the promise, the isolate and the other calls carry on.
    fn expire(&mut self) {
        let now = tokio::time::Instant::now();
        let mut i = 0;
        while i < self.pending.len() {
            if self.pending[i].deadline.is_some_and(|d| d <= now) {
                let call = self.pending.swap_remove(i);
                let verdict = self.disarm(call.guard);
                let _ = call.reply.send(Err(verdict.cutoff()));
            } else {
                i += 1;
            }
        }
    }

    fn memory_exhausted(&mut self) {
        self.oom.store(false, Ordering::SeqCst);
        // With nothing parked the OOM would otherwise vanish: say it.
        if self.pending.is_empty() {
            eprintln!(
                "oj_js: background work exceeded the engine heap limit; the isolate is condemned"
            );
        }
        self.condemn();
    }

    /// An uncaught error broke the event loop (the process would die under
    /// Node): deliver promises that settled on the final turn, fail the rest
    /// with the error.
    fn broken(&mut self, e: deno_core::error::CoreError) {
        let error = e.to_string();
        if self.pending.is_empty() {
            eprintln!("oj_js: uncaught error on the engine event loop: {error}");
        }
        // The oom flag is read ONCE for the whole batch: per-call reads would
        // hand MemoryLimit to whichever call came first and a bare
        // termination error to the rest.
        let oom_fired = self.oom.swap(false, Ordering::SeqCst);
        self.condemned |= oom_fired;
        let mut cx = Context::from_waker(Waker::noop());
        for mut call in std::mem::take(&mut self.pending) {
            // A call whose watchdog terminated the wedged loop is the one
            // that expired. All guards are disarmed BEFORE the one cancel
            // below, so a watchdog firing mid-drain cannot leave a
            // termination pending for the next job.
            let deadline_fired = call.guard.map(DeadlineGuard::disarm).unwrap_or(false);
            let result = match call.fut.as_mut().poll(&mut cx) {
                Poll::Ready(r) => settled_to_json(&mut self.worker, r),
                Poll::Pending if oom_fired => Err(EngineError::MemoryLimit),
                Poll::Pending if deadline_fired => Err(EngineError::Deadline),
                Poll::Pending => Err(EngineError::Js(error.clone())),
            };
            let _ = call.reply.send(result);
        }
        self.worker
            .js_runtime
            .v8_isolate()
            .cancel_terminate_execution();
    }

    /// The tick polls the job channel before the event loop, so an engine
    /// dropped right after its last call closes with V8's freshly produced
    /// code-cache blobs still parked as pending `code_cache_ready` callbacks.
    /// One noop-waker poll invokes them (the loader writes synchronously) and
    /// cannot hang on long-lived background ops the way running the loop to
    /// completion would. The watchdog bounds the JS it may still run:
    /// `JsEngine::drop` joins this thread.
    fn flush_on_close(&mut self) {
        if self.event_loop_idle {
            return;
        }
        let guard = DeadlineGuard::arm(&self.watchdog, Duration::from_millis(250));
        let mut cx = Context::from_waker(Waker::noop());
        let _ = self
            .worker
            .js_runtime
            .poll_event_loop(&mut cx, PollEventLoopOptions::default());
        let _ = guard.disarm();
    }

    async fn run_eval(
        &mut self,
        counter: u64,
        input: EvalInput,
    ) -> Result<serde_json::Value, EngineError> {
        let worker = &mut self.worker;
        let id = match input {
            EvalInput::Source(source) => {
                // Unique synthetic specifier per eval: the module map is
                // per-isolate and persistent, so specifiers must not collide.
                let url = self
                    .root_url
                    .join(&format!("__oj_eval_{counter}__.mjs"))
                    .map_err(js_err)?;
                worker
                    .js_runtime
                    .load_side_es_module_from_code(&url, source)
                    .await
                    .map_err(js_err)?
            }
            EvalInput::Path(path) => {
                let url = resolve_module_path(&self.config, &path)?;
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

    /// Loads and evaluates the module, invokes the export, and returns the
    /// future of its settled result. The future is independent of the worker
    /// borrow, so the scheduler polls it alongside the event loop and other
    /// pending calls.
    async fn setup_call(
        &mut self,
        module: &str,
        export: &str,
        args: Vec<serde_json::Value>,
    ) -> Result<impl Future<Output = SettledResult> + use<>, EngineError> {
        let worker = &mut self.worker;
        let url = resolve_module_spec(&self.config, module)?;
        let id = worker.preload_side_module(&url).await.map_err(js_err)?;
        worker.evaluate_module(id).await.map_err(js_err)?;

        let ns = worker.js_runtime.get_module_namespace(id).map_err(js_err)?;
        let (function, arg_globals) = {
            deno_core::scope!(scope, &mut worker.js_runtime);
            let ns = v8::Local::new(scope, ns);
            let key = v8::String::new(scope, export)
                .ok_or_else(|| EngineError::Js(format!("invalid export name \"{export}\"")))?;
            let value = ns.get(scope, key.into()).ok_or_else(|| {
                EngineError::Js(format!("module {url} has no export \"{export}\""))
            })?;
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
