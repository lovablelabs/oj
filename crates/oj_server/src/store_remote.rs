// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The client side of the oj store's wire (pulp P3): Metro's chain. The local
//! shared store is consulted first (PersistentCache); on a miss, the remote
//! substituter is read through and the entry is written back locally, so the
//! fleet's second sandbox pays one HTTP GET instead of a compile. Pushing is
//! separate and explicit: the dev server pushes only when `OJ_STORE_PUSH=1`
//! AND a token is set — untrusted contexts hold read-only tokens and the
//! server's write-once entries bound what even a leaked write token can do.
//!
//! Trust at the consuming edge: a fetched entry is admitted only if its
//! stored form passes `CachedModule::is_relocatable` (a remote can never pin
//! this worktree into serving another checkout's paths) and only under the
//! key the CLIENT computed (a remote cannot choose which module its bytes
//! stand in for). Entry signing is designed (Nix narinfo model) but not yet
//! wired; until then the write token is the trust boundary.

use std::sync::Arc;
use std::time::Duration;

use oj_cache::CachedModule;

pub struct RemoteStore {
    base: String,
    token: Option<String>,
    push: bool,
    client: reqwest::Client,
    /// Fire-and-forget pushes run on their own small thread to keep the
    /// cache writer thread off the network.
    push_tx: std::sync::mpsc::Sender<(String, Vec<u8>)>,
}

impl RemoteStore {
    /// From `OJ_STORE_REMOTE` (+ `OJ_STORE_TOKEN`, `OJ_STORE_PUSH`): None
    /// when no remote is configured.
    pub fn from_env() -> Option<Arc<RemoteStore>> {
        let knobs = &oj_env::get().knobs;
        let base = knobs.store_remote.as_deref()?;
        let base = base.trim_end_matches('/').to_string();
        let token = knobs.store_token.clone();
        let push = knobs.store_push;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .ok()?;
        let (push_tx, push_rx) = std::sync::mpsc::channel::<(String, Vec<u8>)>();
        let store = Arc::new(RemoteStore {
            base,
            token,
            push,
            client,
            push_tx,
        });
        if store.push {
            let pusher = Arc::clone(&store);
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("push runtime");
                while let Ok((url, body)) = push_rx.recv() {
                    let req = pusher.authed(pusher.client.put(&url)).body(body);
                    // Best-effort: 409 means someone else already published
                    // this key, which is success; errors only cost sharing.
                    let _ = rt.block_on(async { req.send().await });
                }
            });
        }
        Some(store)
    }

    fn authed(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.token {
            Some(t) => req.bearer_auth(t),
            None => req,
        }
    }

    /// Read-through for a module entry this client's key computation missed
    /// locally. Returns the STORED (ws:/mono:) form; the caller rehomes it.
    pub async fn fetch_module(&self, key: &str) -> Option<CachedModule> {
        let url = format!("{}/v1/ac/modules/{key}", self.base);
        let resp = self.authed(self.client.get(&url)).send().await.ok()?;
        if resp.status() != reqwest::StatusCode::OK {
            return None;
        }
        let bytes = resp.bytes().await.ok()?;
        let module: CachedModule = serde_json::from_slice(&bytes).ok()?;
        // The admission gate, applied to what the WIRE delivered: nothing
        // that pins a checkout crosses into this worktree, whatever the
        // server said.
        module.is_relocatable().then_some(module)
    }

    /// Queue a stored-form entry for push. Called from the cache writer
    /// thread; never blocks on the network.
    pub fn push_module(&self, key: &str, stored: &CachedModule) {
        if !self.push {
            return;
        }
        if let Ok(body) = serde_json::to_vec(stored) {
            let url = format!("{}/v1/ac/modules/{key}", self.base);
            let _ = self.push_tx.send((url, body));
        }
    }
}
