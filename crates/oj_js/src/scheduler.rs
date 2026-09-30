// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The engine thread: call jobs run concurrently on the one isolate. A call
//! is set up exclusively (module load + evaluate + invoke), then parked as a
//! pending promise; the loop interleaves accepting new jobs, polling every
//! parked call, and driving the event loop, so a call that awaits a fetch
//! back into the caller (an SSR loader hitting its own dev server) never
//! deadlocks behind itself.
//!
//! A parked call's deadline is part of its future (`tokio::time::timeout_at`),
//! so expiry arrives through the same settlement path as success — there is
//! no separate timer bookkeeping to keep in sync. The watchdog guard stays
//! armed underneath for the one case the timer cannot reach: a continuation
//! wedging the event loop in a busy loop.

use std::future::Future;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use std::time::Duration;

use deno_core::url::Url;
use deno_core::v8;
use tokio::sync::mpsc;
use tokio::time::error::Elapsed;

use crate::bridge::EngineHooks;
use crate::convert::js_err;
use crate::convert::json_to_v8;
use crate::convert::settled_to_json;
use crate::convert::v8_to_json;
use crate::convert::SettledResult;
use crate::engine::Job;
use crate::engine::Reply;
use crate::host::ModuleHost;
use crate::isolate::Isolate;
use crate::isolate::Verdict;
use crate::watchdog::DeadlineGuard;
use crate::EngineConfig;
use crate::EngineError;
use crate::EvalInput;

pub(crate) fn engine_thread(
    config: EngineConfig,
    module_host: Option<ModuleHost>,
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
        let mut scheduler = match Scheduler::boot(config, module_host) {
            Ok(scheduler) => scheduler,
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };
        // Bridge globals and the heap-cap callback are installed only now,
        // with the scheduler at its final address: V8 callback registration
        // must come after the last move of the worker.
        let capped = scheduler.config.memory_limit_bytes.is_some();
        scheduler.isolate.install(hooks, capped);
        if ready.send(Ok(scheduler.isolate.handle())).is_err() {
            return;
        }
        scheduler.run(rx).await;
    });
}

/// How a parked call left the pending set: settled by its promise, or timed
/// out by the deadline baked into its future.
type ParkedOutcome = Result<SettledResult, Elapsed>;

enum Tick {
    Job(Job),
    Settled(usize, ParkedOutcome),
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
/// `guard` is the same deadline on the watchdog, for a wedged event loop.
struct ParkedCall {
    fut: Pin<Box<dyn Future<Output = ParkedOutcome>>>,
    guard: Option<DeadlineGuard>,
    reply: Reply,
}

struct Scheduler {
    isolate: Isolate,
    config: EngineConfig,
    root_url: Url,
    calls: Vec<ParkedCall>,
    /// The event loop drained completely (no ops, no live timers). Only tells
    /// the shutdown path whether a final flush is needed; the loop is polled
    /// on every wake regardless (see [`Scheduler::next_tick`]).
    event_loop_idle: bool,
    eval_counter: u64,
}

impl Scheduler {
    fn boot(
        config: EngineConfig,
        module_host: Option<ModuleHost>,
    ) -> Result<Scheduler, EngineError> {
        let root_url = deno_path_util::url_from_directory_path(&config.root)
            .map_err(|e| EngineError::Boot(e.to_string()))?;
        // Never loaded; MainWorker only needs a main-module identity.
        let main_module = root_url.join("__oj_engine_main__.mjs").unwrap();
        let isolate = Isolate::boot(&config, &main_module, module_host)?;
        Ok(Scheduler {
            isolate,
            config,
            root_url,
            calls: Vec::new(),
            event_loop_idle: false,
            eval_counter: 0,
        })
    }

    async fn run(&mut self, mut rx: mpsc::UnboundedReceiver<Job>) {
        loop {
            let tick = self.next_tick(&mut rx).await;
            if matches!(tick, Tick::Job(_)) {
                self.event_loop_idle = false;
            }
            match tick {
                Tick::Job(job) if self.isolate.is_condemned() => job.refuse_condemned(),
                Tick::Job(Job::Eval {
                    input,
                    deadline,
                    reply,
                }) => self.eval_job(input, deadline, reply).await,
                Tick::Job(Job::Gc { reply }) => {
                    self.isolate.collect_garbage();
                    let _ = reply.send(());
                }
                Tick::Job(Job::Call {
                    module,
                    export,
                    args,
                    deadline,
                    reply,
                }) => self.call_job(module, export, args, deadline, reply).await,
                Tick::Settled(i, outcome) => self.settled(i, outcome),
                Tick::MemoryExhausted => self.memory_exhausted(),
                Tick::Broken(e) => self.broken(e),
                Tick::Closed => {
                    self.flush_on_close();
                    break;
                }
            }
        }
    }

    /// One wake of the engine. Priority is load-bearing and top-down: new
    /// jobs, then parked-call settlements, then event-loop progress (whose
    /// poll also runs V8's own delayed work — the MemoryReducer's GC tasks
    /// that return an idle heap's pages — so it is polled even when drained;
    /// a drained poll just registers the waker, nothing spins). The OOM flag
    /// is read after the poll because the heap-limit callback runs inside it:
    /// the tick never depends on a waker the terminated JS cannot fire.
    async fn next_tick(&mut self, rx: &mut mpsc::UnboundedReceiver<Job>) -> Tick {
        std::future::poll_fn(|cx| {
            match rx.poll_recv(cx) {
                Poll::Ready(Some(job)) => return Poll::Ready(Tick::Job(job)),
                Poll::Ready(None) => return Poll::Ready(Tick::Closed),
                Poll::Pending => {}
            }
            for (i, call) in self.calls.iter_mut().enumerate() {
                if let Poll::Ready(outcome) = call.fut.as_mut().poll(cx) {
                    return Poll::Ready(Tick::Settled(i, outcome));
                }
            }
            match self.isolate.poll_events(cx) {
                Poll::Ready(Ok(())) => self.event_loop_idle = true,
                Poll::Ready(Err(e)) => return Poll::Ready(Tick::Broken(e)),
                Poll::Pending => self.event_loop_idle = false,
            }
            if self.isolate.oom_pending() {
                return Poll::Ready(Tick::MemoryExhausted);
            }
            Poll::Pending
        })
        .await
    }

    /// Settles a finished job's guard and, when the heap cap fired, sweeps
    /// every parked call — their promises can never settle.
    fn settle(&mut self, guard: Option<DeadlineGuard>) -> Verdict {
        let verdict = self.isolate.settle(guard);
        if matches!(verdict, Verdict::MemoryLimit) {
            self.fail_all_parked();
        }
        verdict
    }

    /// Fails every parked call as MemoryLimit and cancels the heap-limit
    /// callback's termination — AFTER all guards are disarmed, so a watchdog
    /// firing mid-sweep cannot leave a termination pending for the next job.
    fn fail_all_parked(&mut self) {
        for call in std::mem::take(&mut self.calls) {
            if let Some(guard) = call.guard {
                let _ = guard.disarm();
            }
            let _ = call.reply.send(Err(EngineError::MemoryLimit));
        }
        self.isolate.cancel_termination();
    }

    async fn eval_job(&mut self, input: EvalInput, deadline: Option<Duration>, reply: Reply) {
        self.eval_counter += 1;
        let counter = self.eval_counter;
        let guard = self.isolate.arm(deadline);
        let result = self.run_eval(counter, input).await;
        let verdict = self.settle(guard);
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
        // termination can never hit an innocent job; while the call is
        // parked, its future's own timeout normally settles the deadline and
        // the watchdog only matters for a wedged event loop.
        let deadline_at = deadline.map(|d| tokio::time::Instant::now() + d);
        let guard = self.isolate.arm(deadline);
        match self.setup_call(&module, &export, args).await {
            Ok(_) if guard.as_ref().is_some_and(DeadlineGuard::fired) => {
                // Setup outlived the deadline but completed anyway (the
                // termination raced completion): expired, not broken.
                let verdict = self.settle(guard);
                let _ = reply.send(Err(verdict.cutoff()));
            }
            Ok(fut) => {
                let fut: Pin<Box<dyn Future<Output = ParkedOutcome>>> = match deadline_at {
                    Some(at) => Box::pin(tokio::time::timeout_at(at, fut)),
                    None => Box::pin(async move { Ok(fut.await) }),
                };
                self.calls.push(ParkedCall { fut, guard, reply });
            }
            Err(e) => {
                let verdict = self.settle(guard);
                let _ = reply.send(verdict.apply(Err(e)));
            }
        }
    }

    fn settled(&mut self, i: usize, outcome: ParkedOutcome) {
        let call = self.calls.swap_remove(i);
        let result = match outcome {
            Ok(settled) => Some(settled_to_json(&mut self.isolate.worker, settled)),
            // Timed out while parked: dropping the future abandons the
            // promise; the isolate and the other calls carry on.
            Err(_elapsed) => None,
        };
        let verdict = self.settle(call.guard);
        let _ = call.reply.send(match result {
            Some(result) => verdict.apply(result),
            None => Err(verdict.cutoff()),
        });
    }

    fn memory_exhausted(&mut self) {
        let _ = self.isolate.take_oom();
        // With nothing parked the OOM would otherwise vanish: say it.
        if self.calls.is_empty() {
            eprintln!(
                "oj_js: background work exceeded the engine heap limit; the isolate is condemned"
            );
        }
        self.fail_all_parked();
    }

    /// An uncaught error broke the event loop (the process would die under
    /// Node): deliver promises that settled on the final turn, fail the rest
    /// with the error.
    fn broken(&mut self, e: deno_core::error::CoreError) {
        let error = e.to_string();
        if self.calls.is_empty() {
            eprintln!("oj_js: uncaught error on the engine event loop: {error}");
        }
        // The OOM flag is read ONCE for the whole batch: per-call reads would
        // hand MemoryLimit to whichever call came first and a bare
        // termination error to the rest.
        let oom_fired = self.isolate.take_oom();
        let mut cx = Context::from_waker(Waker::noop());
        for mut call in std::mem::take(&mut self.calls) {
            // A call whose watchdog terminated the wedged loop is the one
            // that expired. All guards are disarmed BEFORE the one cancel
            // below, so a watchdog firing mid-drain cannot leave a
            // termination pending for the next job.
            let deadline_fired = call.guard.map(DeadlineGuard::disarm).unwrap_or(false);
            let result = match call.fut.as_mut().poll(&mut cx) {
                Poll::Ready(Ok(settled)) => settled_to_json(&mut self.isolate.worker, settled),
                Poll::Ready(Err(_)) if oom_fired => Err(EngineError::MemoryLimit),
                Poll::Ready(Err(_)) => Err(EngineError::Deadline),
                Poll::Pending if oom_fired => Err(EngineError::MemoryLimit),
                Poll::Pending if deadline_fired => Err(EngineError::Deadline),
                Poll::Pending => Err(EngineError::Js(error.clone())),
            };
            let _ = call.reply.send(result);
        }
        self.isolate.cancel_termination();
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
        let guard = self.isolate.arm(Some(Duration::from_millis(250)));
        let mut cx = Context::from_waker(Waker::noop());
        let _ = self.isolate.poll_events(&mut cx);
        if let Some(guard) = guard {
            let _ = guard.disarm();
        }
    }

    async fn run_eval(
        &mut self,
        counter: u64,
        input: EvalInput,
    ) -> Result<serde_json::Value, EngineError> {
        let worker = &mut self.isolate.worker;
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
    /// parked calls.
    async fn setup_call(
        &mut self,
        module: &str,
        export: &str,
        args: Vec<serde_json::Value>,
    ) -> Result<impl Future<Output = SettledResult> + use<>, EngineError> {
        let worker = &mut self.isolate.worker;
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
