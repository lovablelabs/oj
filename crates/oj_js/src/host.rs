// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The module-host seam: the dev server answers the engine's resolve/load
//! requests over a channel — the role oj's SSR and Start hosts play for
//! engine-run modules (transform pipelines, virtual modules, invalidation via
//! version-stamped specifiers). A concrete request enum, not a trait: the
//! host is whoever holds the receiving end, and it serves requests on its own
//! runtime, so the engine needs no runtime handle and no dynamic dispatch.

use tokio::sync::mpsc;
use tokio::sync::oneshot;

/// A resolution the host made for an import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostResolved {
    /// A fully resolved module URL the host will serve through a
    /// [`HostRequest::Load`]. Must parse as an absolute URL; the host owns
    /// the scheme and any cache-busting query (e.g. `?v=N` version stamps).
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

/// One question from the engine's loader. Replying `Ok(None)` defers to the
/// engine's built-in behavior (byonm Node resolution, filesystem loading).
pub enum HostRequest {
    /// Sees every import except absolute `file:`/`node:`/`data:`/`blob:`
    /// URLs. deno_core's resolve is synchronous, so the isolate thread PARKS
    /// on this reply: the server must answer from a runtime that keeps
    /// running meanwhile (never the engine's own thread).
    Resolve {
        importer: String,
        specifier: String,
        reply: std::sync::mpsc::Sender<Result<Option<HostResolved>, String>>,
    },
    /// Sees every module fetch except `node:` builtins. Awaited on the
    /// engine's async load path; several may be in flight at once.
    Load {
        specifier: String,
        reply: oneshot::Sender<Result<Option<HostModule>, String>>,
    },
}

/// The stream of requests a host serves; see [`ModuleHost::channel`]. Ends
/// (recv returns `None`) when the engine is gone.
pub type HostRequests = mpsc::UnboundedReceiver<HostRequest>;

/// The engine's end of the module-host channel, given to
/// [`crate::JsEngine::spawn`].
#[derive(Clone)]
pub struct ModuleHost {
    tx: mpsc::UnboundedSender<HostRequest>,
}

impl ModuleHost {
    pub fn channel() -> (ModuleHost, HostRequests) {
        let (tx, rx) = mpsc::unbounded_channel();
        (ModuleHost { tx }, rx)
    }

    pub(crate) fn resolve_blocking(
        &self,
        importer: &str,
        specifier: &str,
    ) -> Result<Option<HostResolved>, String> {
        let (reply, reply_rx) = std::sync::mpsc::channel();
        self.tx
            .send(HostRequest::Resolve {
                importer: importer.to_string(),
                specifier: specifier.to_string(),
                reply,
            })
            .map_err(|_| "the module host is gone".to_string())?;
        reply_rx
            .recv()
            .map_err(|_| "the module host dropped the resolve reply".to_string())?
    }

    pub(crate) async fn load(&self, specifier: &str) -> Result<Option<HostModule>, String> {
        let (reply, reply_rx) = oneshot::channel();
        self.tx
            .send(HostRequest::Load {
                specifier: specifier.to_string(),
                reply,
            })
            .map_err(|_| "the module host is gone".to_string())?;
        reply_rx
            .await
            .map_err(|_| "the module host dropped the load reply".to_string())?
    }
}
