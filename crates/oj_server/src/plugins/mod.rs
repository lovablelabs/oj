// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use oj_resolver::OjResolver;

pub const PLUGIN_HOST_JS: &str = include_str!("../assets/plugin-host.mjs");
/// Sibling module the plugin host and preseed optimizer child import as `./discovered-deps.mjs`.
pub const DISCOVERED_DEPS_JS: &str = include_str!("../assets/discovered-deps.mjs");

/// Idempotent, atomic materialization of an embedded asset into `dir`.
pub(crate) fn ensure_asset(dir: &Path, name: &str, bytes: &str) -> std::io::Result<()> {
    let path = dir.join(name);
    if std::fs::read(&path).ok().as_deref() == Some(bytes.as_bytes()) {
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("{name}.tmp-{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, &path)
}
pub const VITE_EXTRACT_JS: &str = include_str!("../assets/vite-extract.mjs");

/// The host's `getServeInfo` report: how requests are served.
#[derive(Debug, Default, Clone, Copy)]
pub struct ServeInfo {
    /// Loopback port of the configureServer middleware stack, when any plugin
    /// registered a middleware.
    pub middleware_port: Option<u16>,
    /// Real runner-backed Vite DevEnvironments were built (the Environment-API
    /// path): documents are served by the plugin middleware.
    pub runner_environments: bool,
}

impl ServeInfo {
    /// The `{ middlewarePort, runnerEnvironments }` shape, shared by the host's
    /// `getServeInfo` RPC reply and its `{ ojServeInfo: ... }` stdout push.
    pub(crate) fn from_json(v: &serde_json::Value) -> ServeInfo {
        ServeInfo {
            middleware_port: v
                .get("middlewarePort")
                .and_then(|p| p.as_u64())
                .and_then(|p| u16::try_from(p).ok()),
            runner_environments: v
                .get("runnerEnvironments")
                .and_then(|b| b.as_bool())
                .unwrap_or(false),
        }
    }
}

#[derive(Debug)]
pub struct EmittedFile {
    pub file_name: String,
    pub source: String,
}

/// A chunk a plugin asked oj to emit via `this.emitFile({ type: "chunk" })`.
#[derive(Debug, Clone)]
pub struct ChunkEmit {
    pub ref_id: String,
    pub id: String,
    pub name: Option<String>,
    pub file_name: Option<String>,
}

impl ChunkEmit {
    fn from_value(m: &serde_json::Value) -> Option<Self> {
        Some(Self {
            ref_id: m.get("referenceId")?.as_str()?.to_string(),
            id: m.get("id")?.as_str()?.to_string(),
            name: m.get("name").and_then(|x| x.as_str()).map(str::to_string),
            file_name: m
                .get("fileName")
                .and_then(|x| x.as_str())
                .map(str::to_string),
        })
    }
}

#[inline]
pub fn plugins_file(root: &Path) -> Option<std::path::PathBuf> {
    ["oj.plugins.mjs", "oj.plugins.js"]
        .into_iter()
        .map(|f| root.join(f))
        .find(|p| p.is_file())
}

pub enum PluginSource {
    OjPlugins(std::path::PathBuf),
    ViteConfig(std::path::PathBuf),
}

#[inline]
/// `config` is the CLI's `--config` (resolved against the app root); named, it
/// replaces Vite's default probe entirely.
pub fn vite_config_file(root: &Path, config: Option<&Path>) -> Option<std::path::PathBuf> {
    if let Some(p) = config {
        return p.is_file().then(|| p.to_path_buf());
    }
    // Vite's DEFAULT_CONFIG_FILES order: first existing wins.
    [
        "vite.config.js",
        "vite.config.mjs",
        "vite.config.ts",
        "vite.config.cjs",
        "vite.config.mts",
        "vite.config.cts",
    ]
    .into_iter()
    .map(|f| root.join(f))
    .find(|p| p.is_file())
}

#[inline]
pub fn plugin_source(root: &Path, config: Option<&Path>) -> Option<PluginSource> {
    if config.is_some() {
        return vite_config_file(root, config).map(PluginSource::ViteConfig);
    }
    if let Some(p) = plugins_file(root) {
        return Some(PluginSource::OjPlugins(p));
    }
    vite_config_file(root, config).map(PluginSource::ViteConfig)
}

mod extract;
mod hook_plan;
mod host;
pub use self::extract::*;
pub use self::hook_plan::*;
pub use self::host::*;

// Extraction contract through the REAL in-process engine: one V8 isolate per
// case, serialized on one lock (env-knob tests must hold it too).
#[cfg(test)]
mod engine_extraction_tests;
#[cfg(test)]
mod hook_plan_tests;
#[cfg(test)]
mod vite_values_tests;
