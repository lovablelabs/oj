// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! In-process JavaScript engine for oj, backed by Deno (deno_core +
//! deno_runtime) with Node compatibility over the app's own node_modules.
//!
//! A [`JsEngine`] is a handle to a dedicated OS thread that owns a
//! `MainWorker` (a V8 isolate is `!Send`, so it can never leave that thread).
//! The handle itself is `Send + Sync`: callers submit jobs over a channel and
//! await the reply. One isolate per thread only — V8 aborts the process when
//! two runtimes are dropped on the same thread.

mod bridge;
pub mod code_cache;
/// The engine's OpenTelemetry pipeline, the very crate (and so the very
/// `OTEL_GLOBALS`) the runtime was built with.
pub use deno_runtime::deno_telemetry;
mod convert;
mod engine;
mod host;
mod isolate;
mod loader;
mod package_json_memo;
mod scheduler;
mod stand_ins;
mod watchdog;
mod worker;

/// The heap limit (young and old generation together, in bytes) V8 gives an
/// isolate on a machine with `system_memory` bytes: its own
/// `ResourceConstraints::ConfigureDefaults`, which is how Node sizes its heap
/// and how Deno's CLI does (`create_isolate_create_params`). `max_old_space_mb`
/// is Node's `--max-old-space-size`: it replaces the old generation's size and
/// leaves the young generation V8 chose.
pub fn heap_limit_bytes(system_memory: u64, max_old_space_mb: Option<usize>) -> usize {
    let mut params =
        deno_core::v8::CreateParams::default().heap_limits_from_system_memory(system_memory, 0);
    if let Some(mb) = max_old_space_mb {
        params = params.set_max_old_generation_size_in_bytes(mb * 1024 * 1024);
    }
    params.max_old_generation_size_in_bytes() + params.max_young_generation_size_in_bytes()
}

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
pub use engine::EngineRegistry;
pub use engine::JsEngine;
pub use host::HostModule;
pub use host::HostModuleType;
pub use host::HostRequest;
pub use host::HostRequests;
pub use host::HostResolved;
pub use host::ModuleHost;

use std::path::PathBuf;
use std::time::Duration;

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
    /// its own. It stops running JS only while no other call is parked (see
    /// [`JsEngine::call`]); engines that multiplex calls set none, like Vite.
    pub default_deadline: Option<Duration>,
    /// Persistent V8 code-cache directory (see [`code_cache`]). The caller
    /// keys the directory by [`engine_abi_key`]; entries self-invalidate on
    /// source change. Best-effort: a broken or read-only cache only costs the
    /// speedup.
    pub code_cache_dir: Option<PathBuf>,
    /// Joins this [`EngineRegistry`] at spawn, so the owner's memory probe
    /// can fan a GC over the engine. Long-lived engines set it; one-shots
    /// leave it out.
    pub registry: Option<EngineRegistry>,
}

impl EngineConfig {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            memory_limit_bytes: None,
            default_deadline: None,
            code_cache_dir: None,
            registry: None,
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
    /// The evaluation was cut short from outside the program: the process
    /// running it was killed by a signal. Nothing about the program failed,
    /// so a caller may run it again.
    #[error("the JS evaluation was interrupted: {0}")]
    Interrupted(String),
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

/// Locks a mutex, riding through poisoning: engine state stays usable after
/// a panicking thread.
pub(crate) fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
