// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The isolate and its health model, behind one type: the heap-cap OOM flag,
//! the deadline watchdog, and the condemned latch always move together, so
//! the scheduler cannot settle a job without also un-poisoning the isolate.

use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;

use deno_core::url::Url;
use deno_core::v8;
use deno_core::PollEventLoopOptions;
use deno_runtime::worker::MainWorker;

use crate::bridge;
use crate::bridge::EngineHooks;
use crate::host::ModuleHost;
use crate::watchdog::DeadlineGuard;
use crate::watchdog::Watchdog;
use crate::worker::build_worker;
use crate::EngineConfig;
use crate::EngineError;

/// What a finished job's termination state (watchdog, heap cap) says about
/// its outcome.
pub(crate) enum Verdict {
    Clean,
    Deadline,
    MemoryLimit,
}

impl Verdict {
    /// Maps a job error onto the termination that caused it.
    pub(crate) fn apply(
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
    pub(crate) fn cutoff(self) -> EngineError {
        match self {
            Verdict::MemoryLimit => EngineError::MemoryLimit,
            _ => EngineError::Deadline,
        }
    }
}

pub(crate) struct Isolate {
    pub(crate) worker: MainWorker,
    watchdog: Watchdog,
    /// Set by the near-heap-limit callback, which runs inside an event-loop
    /// poll; read right after each poll so exhaustion never depends on a
    /// waker the terminated JS cannot fire.
    oom: Arc<AtomicBool>,
    /// The heap cap fired: the isolate ran on and may have lost arbitrary
    /// state, so every later job fails fast as MemoryLimit until the owner
    /// (CSS revive, plugin-host respawn) replaces the engine. Without it,
    /// each background-work OOM permanently doubles the limit.
    condemned: bool,
}

impl Isolate {
    pub(crate) fn boot(
        config: &EngineConfig,
        main_module: &Url,
        module_host: Option<ModuleHost>,
    ) -> Result<Isolate, EngineError> {
        let mut worker = build_worker(config, main_module, module_host)?;
        let oom = Arc::new(AtomicBool::new(false));
        let watchdog = Watchdog::spawn(worker.js_runtime.v8_isolate().thread_safe_handle());

        Ok(Isolate {
            worker,
            watchdog,
            oom,
            condemned: false,
        })
    }

    pub(crate) fn handle(&mut self) -> v8::IsolateHandle {
        self.worker.js_runtime.v8_isolate().thread_safe_handle()
    }

    /// Second boot phase, run AFTER the isolate reached its final address:
    /// installs the bridge globals and the near-heap-limit callback.
    pub(crate) fn install(&mut self, hooks: Option<EngineHooks>, capped: bool) {
        if let Some(hooks) = hooks {
            bridge::install(&mut self.worker, hooks);
        }
        if capped {
            let handle = self.worker.js_runtime.v8_isolate().thread_safe_handle();
            let oom = self.oom.clone();
            self.worker
                .js_runtime
                .add_near_heap_limit_callback(move |current, _initial| {
                    oom.store(true, Ordering::SeqCst);
                    handle.terminate_execution();
                    // Raise the limit so V8 can unwind while the termination
                    // lands, instead of aborting the process.
                    current * 2
                });
        }
    }

    pub(crate) fn is_condemned(&self) -> bool {
        self.condemned
    }

    pub(crate) fn arm(&self, deadline: Option<Duration>) -> Option<DeadlineGuard> {
        deadline.map(|d| DeadlineGuard::arm(&self.watchdog, d))
    }

    /// Settles a finished job's guard against the OOM flag, un-poisoning the
    /// isolate so later jobs run. A fired heap cap latches the condemned
    /// state; the caller sweeps its parked calls on `Verdict::MemoryLimit`.
    pub(crate) fn settle(&mut self, guard: Option<DeadlineGuard>) -> Verdict {
        let deadline_fired = guard.map(DeadlineGuard::disarm).unwrap_or(false);
        let oom_fired = self.take_oom();
        if deadline_fired || oom_fired {
            self.cancel_termination();
        }
        if oom_fired {
            self.condemned = true;
            Verdict::MemoryLimit
        } else if deadline_fired {
            Verdict::Deadline
        } else {
            Verdict::Clean
        }
    }

    /// Consumes the OOM flag; `true` latches the condemned state.
    /// Peeks the OOM flag without consuming it. The tick must PEEK, never
    /// swap: an RMW through `&mut self` inside the tick closure (which has
    /// just been through the V8 FFI of `poll_events`) miscompiled in release
    /// builds — every job on an uncapped engine came back MemoryLimit
    /// (bisected, stable across layout perturbations). The flag is consumed
    /// outside the tick, by `settle` and the MemoryExhausted arm.
    pub(crate) fn oom_pending(&self) -> bool {
        self.oom.load(Ordering::SeqCst)
    }

    pub(crate) fn take_oom(&mut self) -> bool {
        let fired = self.oom.swap(false, Ordering::SeqCst);
        if fired {
            self.condemned = true;
        }
        fired
    }

    pub(crate) fn cancel_termination(&mut self) {
        self.worker
            .js_runtime
            .v8_isolate()
            .cancel_terminate_execution();
    }

    pub(crate) fn collect_garbage(&mut self) {
        self.worker
            .js_runtime
            .v8_isolate()
            .low_memory_notification();
    }

    pub(crate) fn poll_events(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), deno_core::error::CoreError>> {
        self.worker
            .js_runtime
            .poll_event_loop(cx, PollEventLoopOptions::default())
    }
}
