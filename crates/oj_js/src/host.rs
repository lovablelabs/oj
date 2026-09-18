// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The module-host seam: a pluggable, async authority the engine consults
//! before its own byonm loader.
//!
//! A [`ModuleHost`] resolves import specifiers and serves module code — the
//! role oj's dev server plays for SSR modules (transform pipelines, virtual
//! modules, invalidation via version-stamped specifiers). Implementations are
//! ordinary async code: the engine's loader lives inside the isolate thread's
//! current-thread runtime, so it never awaits host futures directly. Instead
//! every call is spawned onto the tokio runtime the engine was created from
//! (see [`HostBridge`]) and the reply travels back over a channel.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Boxed future returned by [`ModuleHost`] methods, so the trait stays
/// object-safe while implementations write ordinary async blocks.
pub type HostFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A resolution the host made for an import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostResolved {
    /// A fully resolved module URL the host will serve through
    /// [`ModuleHost::load`]. Must parse as an absolute URL; the host owns the
    /// scheme and any cache-busting query (e.g. `?v=N` version stamps).
    Url(String),
    /// Not the host's module: the engine resolves this specifier with its own
    /// Node semantics (bare npm specifiers, node_modules internals).
    External(String),
}

/// Code the host serves for one of its module URLs.
#[derive(Debug, Clone)]
pub struct HostModule {
    pub code: String,
    pub module_type: HostModuleType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostModuleType {
    JavaScript,
    Json,
}

/// Async module authority consulted by the engine's loader.
///
/// `resolve` sees every import except absolute `file:`/`node:`/`data:`/`blob:`
/// URLs; `load` sees every module fetch except `node:` builtins. Returning
/// `None` from either defers to the engine's built-in behavior (byonm Node
/// resolution and filesystem loading).
pub trait ModuleHost: Send + Sync + 'static {
    fn resolve<'a>(
        &'a self,
        importer: &'a str,
        specifier: &'a str,
    ) -> HostFuture<'a, Result<Option<HostResolved>, String>>;

    fn load<'a>(&'a self, specifier: &'a str)
        -> HostFuture<'a, Result<Option<HostModule>, String>>;
}

/// Runs [`ModuleHost`] futures on the runtime the engine was spawned from and
/// ferries replies to the engine thread.
///
/// deno_core's `ModuleLoader::resolve` is synchronous and runs on the isolate
/// thread while its event loop is being polled, so `resolve_blocking` parks
/// that thread on a plain channel (never `block_on`); the host future runs
/// elsewhere, on the multi-thread runtime, so this cannot self-deadlock.
/// `load` is consulted from the loader's async path and awaits normally.
#[derive(Clone)]
pub(crate) struct HostBridge {
    runtime: tokio::runtime::Handle,
    host: Arc<dyn ModuleHost>,
}

impl HostBridge {
    pub(crate) fn new(runtime: tokio::runtime::Handle, host: Arc<dyn ModuleHost>) -> Self {
        HostBridge { runtime, host }
    }

    pub(crate) fn resolve_blocking(
        &self,
        importer: &str,
        specifier: &str,
    ) -> Result<Option<HostResolved>, String> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        let host = self.host.clone();
        let importer = importer.to_string();
        let specifier = specifier.to_string();
        self.runtime.spawn(async move {
            let _ = reply_tx.send(host.resolve(&importer, &specifier).await);
        });
        reply_rx
            .recv()
            .map_err(|_| "module host dropped the resolve reply".to_string())?
    }

    pub(crate) async fn load(&self, specifier: &str) -> Result<Option<HostModule>, String> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let host = self.host.clone();
        let specifier = specifier.to_string();
        self.runtime.spawn(async move {
            let _ = reply_tx.send(host.load(&specifier).await);
        });
        reply_rx
            .await
            .map_err(|_| "module host dropped the load reply".to_string())?
    }
}
