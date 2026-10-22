// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The plugin hosts a Start bundle runs the app's Vite plugins in: one per
//! Vite environment, in Start bundle mode (the framework plugins oj
//! reimplements left out, a plugin's `this.resolve` ending in the bundle's
//! own resolution).

use std::path::Path;
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
    let facts = Facts::from_json(
        &host
            .start_facts()
            .await
            .map_err(|e| anyhow::anyhow!("the {env_name} plugin host: {e}"))?,
    );
    let defines = host.config_defines().await;
    let env_delta = host.env_delta().await.into_iter().collect();
    Ok(Some(BundleHost {
        host,
        ctx,
        facts,
        defines,
        env_delta,
    }))
}
