// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The plugin hosts a Start bundle runs the app's Vite plugins in: one per
//! Vite environment, in Start bundle mode (the framework plugins oj
//! reimplements left out, a plugin's `this.resolve` ending in the bundle's
//! own resolution).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use oj_server::plugins::PluginHost;

use super::facts::Facts;
use super::host::{serve_bundler_resolve, BundlerCtx};
use super::{Command, StartConfig};

/// One environment's host, with what the bundle reads off it.
#[derive(Clone)]
pub(crate) struct BundleHost {
    pub host: Arc<PluginHost>,
    pub ctx: BundlerCtx,
    pub facts: Facts,
    /// `define`s the plugins' config hooks contributed.
    pub defines: Vec<(String, String)>,
    /// `process.env` values the plugins' config hooks set.
    pub env_delta: Vec<(String, String)>,
}

/// Spawns the host for `env_name`, or `None` when the app has no plugin
/// source (no Vite config, no oj plugins file). A host that fails to start
/// fails the bundle: its plugins' output would be missing otherwise.
pub(crate) async fn spawn(
    start: &StartConfig,
    config_file: Option<&Path>,
    env_name: &str,
) -> anyhow::Result<Option<BundleHost>> {
    let Some(source) = oj_server::plugins::plugin_source(&start.root, config_file) else {
        return Ok(None);
    };
    let (file, format) = match source {
        oj_server::plugins::PluginSource::OjPlugins(p) => (p, "oj"),
        oj_server::plugins::PluginSource::ViteConfig(p) => (p, "vite"),
    };
    let command = match start.command {
        Command::Serve => "serve",
        Command::Build => "build",
    };
    let payload = serde_json::json!({
        "config": {
            "root": start.root.display().to_string(),
            "base": start.base,
            "mode": start.mode,
            "command": command,
            "define": start.config.define,
            "environments": start.config.environments.clone().unwrap_or_default(),
        },
        "env": { "command": command, "mode": start.mode },
        "environment": {
            "name": env_name,
            "mode": if start.command == Command::Build { "build" } else { "dev" },
        },
        "pluginsFormat": format,
        "ojStartMode": true,
        "ojBundleHost": true,
    })
    .to_string();
    let host = PluginHost::spawn(&start.root, &file, &payload, None)
        .await
        .map_err(|e| anyhow::anyhow!("the {env_name} plugin host failed to start: {e}"))?;
    let ctx = serve_bundler_resolve(&host);
    let (facts, defines, env_delta) = read_facts(&host, env_name).await?;
    Ok(Some(BundleHost {
        host,
        ctx,
        facts,
        defines,
        env_delta,
    }))
}

async fn read_facts(
    host: &PluginHost,
    env_name: &str,
) -> anyhow::Result<(Facts, Vec<(String, String)>, Vec<(String, String)>)> {
    let facts = Facts::from_json(
        &host
            .start_facts()
            .await
            .map_err(|e| anyhow::anyhow!("the {env_name} plugin host: {e}"))?,
    );
    let defines = host.config_defines().await;
    let env_delta = host.env_delta().await.into_iter().collect();
    Ok((facts, defines, env_delta))
}

/// See [`BundleHost::close_on_drop`].
pub(crate) struct ContainerGuard(Option<Arc<PluginHost>>);

impl ContainerGuard {
    pub fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for ContainerGuard {
    fn drop(&mut self) {
        let Some(host) = self.0.take() else {
            return;
        };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = host.close_container().await;
            });
        }
    }
}

impl BundleHost {
    /// The same host for the next environment's build (Vite's
    /// `builder.sharedPlugins`): its plugin set, facts and defines.
    pub async fn switch_environment(&self, env_name: &str) -> anyhow::Result<BundleHost> {
        self.host
            .switch_environment(env_name)
            .await
            .map_err(|e| anyhow::anyhow!("the {env_name} plugin host: {e}"))?;
        let (facts, defines, env_delta) = read_facts(&self.host, env_name).await?;
        Ok(BundleHost {
            host: Arc::clone(&self.host),
            ctx: self.ctx.clone(),
            facts,
            defines,
            env_delta,
        })
    }

    /// A worker entry's bundle on this host (Vite's bundleWorkerEntry): a
    /// container of its own over `worker.plugins`, a fresh set beside the
    /// app's plugins, its resolved config `isWorker` with `chain`. Its
    /// `this.resolve` ends in the worker's own bundle. [`BundleHost::close_worker`]
    /// ends it; a container that fails to start is closed here.
    pub async fn worker(&self, chain: &[PathBuf]) -> anyhow::Result<BundleHost> {
        let chain: Vec<String> = chain.iter().map(|p| p.display().to_string()).collect();
        let host = Arc::new(
            self.host
                .open_worker_container(&chain)
                .await
                .map_err(|e| anyhow::anyhow!("the worker plugin container: {e}"))?,
        );
        let ctx = serve_bundler_resolve(&host);
        // Only the facts: a worker bundle takes the client's defines and env.
        let facts = match host.start_facts().await {
            Ok(facts) => Facts::from_json(&facts),
            Err(e) => {
                let _ = host.close_container().await;
                return Err(anyhow::anyhow!("the worker plugin container: {e}"));
            }
        };
        Ok(BundleHost {
            host,
            ctx,
            facts,
            defines: Vec::new(),
            env_delta: Vec::new(),
        })
    }

    /// Closes this worker's container if dropped armed: a build cancelled
    /// mid-bundle never reaches [`BundleHost::close_worker`].
    pub fn close_on_drop(&self) -> ContainerGuard {
        ContainerGuard(Some(Arc::clone(&self.host)))
    }

    /// A worker bundle's closeBundle, then its container closed.
    pub async fn close_worker(&self) -> Result<(), String> {
        let closed = self.host.close_bundle().await;
        let _ = self.host.close_container().await;
        closed.map_err(|e| format!("plugin closeBundle failed:\n{e}"))
    }
}
