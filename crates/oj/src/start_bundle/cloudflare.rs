// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The Cloudflare Worker build. With @cloudflare/vite-plugin in the app's
//! config, `vite build` produces no Node server: the plugin's `config` hook
//! declares one Vite environment per Worker, bundled for workerd (`platform:
//! "neutral"`, the `cloudflare:*` modules and, under nodejs_compat, the
//! runtime's `node:*` ones left external, everything else inlined), and its
//! output-config plugin writes the deployable `wrangler.json` from
//! `generateBundle` / `writeBundle`. oj runs those hooks through the Worker
//! environment's plugin host; this module supplies the rolldown side.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use oj_server::plugins::PluginHost;
use rolldown_common::ResolvedExternal;
use rolldown_plugin::__inner::{Pluginable, SharedPluginable};
use rolldown_plugin::{
    HookResolveIdArgs, HookResolveIdOutput, HookResolveIdReturn, Plugin, PluginContext,
};

use super::facts::CloudflareEnv;

pub(crate) const WORKER_ENTRY: &str = "virtual:cloudflare/worker-entry";
const USER_ENTRY: &str = "virtual:cloudflare/user-entry";

/// Whether `id` names one of Node's built-ins (`node:x`, or a bare name).
fn is_node_builtin(id: &str) -> bool {
    id.strip_prefix("node:")
        .is_some_and(|rest| !rest.is_empty())
        || (super::shims::is_bare_builtin(id) && !id.starts_with('_'))
}

/// The unenv polyfills the plugin's nodejs-compat hook resolves to.
fn is_node_polyfill(id: &str) -> bool {
    id.starts_with("unenv/") || id.starts_with("@cloudflare/unenv-preset/")
}

/// TanStack Start's default Worker entry (`main:
/// "@tanstack/react-start/server-entry"`), which oj replaces with its own
/// server entry as it does for the Node build.
fn is_start_default_entry(id: &str) -> bool {
    let id = id.replace('\\', "/");
    id.contains("/@tanstack/react-start/dist/default-entry/")
}

pub(crate) struct CloudflareWorkerPlugin {
    host: Arc<PluginHost>,
    env: CloudflareEnv,
    server_entry: PathBuf,
}

impl std::fmt::Debug for CloudflareWorkerPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CloudflareWorkerPlugin")
    }
}

impl CloudflareWorkerPlugin {
    pub fn new(host: Arc<PluginHost>, env: CloudflareEnv, server_entry: PathBuf) -> Self {
        Self {
            host,
            env,
            server_entry,
        }
    }
}

impl Plugin for CloudflareWorkerPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("oj:cloudflare-worker")
    }

    fn register_hook_usage(&self) -> rolldown_plugin::HookUsage {
        rolldown_plugin::HookUsage::ResolveId
    }

    async fn resolve_id(
        &self,
        _ctx: &PluginContext,
        args: &HookResolveIdArgs<'_>,
    ) -> HookResolveIdReturn {
        let source = args.specifier;
        let importer = args.importer.unwrap_or("");
        if source == USER_ENTRY {
            // The plugin resolves the wrangler `main`; Start's default entry
            // becomes oj's.
            let main = self
                .host
                .resolve_id_phase(source, importer, "all", false)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "could not resolve the Worker's main entry (wrangler config `main`)"
                    )
                })?;
            let id = if is_start_default_entry(&main.id) {
                self.server_entry.to_string_lossy().into_owned()
            } else {
                main.id
            };
            return Ok(Some(HookResolveIdOutput::from_id(id)));
        }
        if source.starts_with("cloudflare:") {
            return Ok(Some(external(source)));
        }
        if !is_node_builtin(source) && !is_node_polyfill(source) {
            return Ok(None);
        }
        if let Some(r) = self
            .host
            .resolve_id_phase(source, importer, "all", false)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?
        {
            return Ok(Some(if r.is_external() {
                external(&r.id)
            } else {
                HookResolveIdOutput::from_id(r.id)
            }));
        }
        Ok(self.env.is_builtin(source).then(|| external(source)))
    }
}

fn external(id: &str) -> HookResolveIdOutput {
    HookResolveIdOutput {
        external: Some(ResolvedExternal::Bool(true)),
        ..HookResolveIdOutput::from_id(id)
    }
}

/// The rolldown builtin plugins the Worker environment attached, as oj's
/// linked rolldown provides them; an unknown one is reported and skipped.
pub(crate) fn builtin_plugins(env: &CloudflareEnv) -> Vec<SharedPluginable> {
    let mut out: Vec<SharedPluginable> = Vec::new();
    for (name, options) in &env.rolldown_plugins {
        match name.as_str() {
            "builtin:esm-external-require" => {
                let external = options
                    .get("external")
                    .and_then(|e| e.as_array())
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|e| match e {
                                serde_json::Value::String(s) => {
                                    Some(rolldown_utils::pattern_filter::StringOrRegex::String(s.clone()))
                                }
                                other => super::facts::js_regex(other)
                                    .map(rolldown_utils::pattern_filter::StringOrRegex::Regex),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                out.push(Pluginable::new_shared(
                    rolldown_plugin_esm_external_require::EsmExternalRequirePlugin {
                        external,
                        skip_duplicate_check: options
                            .get("skipDuplicateCheck")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false),
                    },
                ));
            }
            other => eprintln!(
                "oj build: (!) the Cloudflare Worker environment's rolldown plugin {other} has no oj counterpart; skipped"
            ),
        }
    }
    out
}

/// Where the Worker files go: the environment's `build.outDir` (root
/// relative, as the plugin resolves it), kept at the same place under oj's
/// output root when `--out` moved that root.
pub(crate) fn worker_out_dir(env: &CloudflareEnv, root: &Path, dist: &Path) -> PathBuf {
    let root_out = root.join(env.root_out_dir.as_deref().unwrap_or("dist"));
    let declared = match &env.out_dir {
        Some(d) => root.join(d),
        None => root_out.join(&env.name),
    };
    let declared = normalize(&declared);
    match declared.strip_prefix(normalize(&root_out)) {
        Ok(rel) if !rel.as_os_str().is_empty() => dist.join(rel),
        _ => declared,
    }
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(out_dir: Option<&str>, root_out: Option<&str>) -> CloudflareEnv {
        CloudflareEnv {
            name: "my_worker".into(),
            builtins: Vec::new(),
            conditions: Vec::new(),
            main_fields: None,
            target: None,
            rolldown_plugins: Vec::new(),
            out_dir: out_dir.map(str::to_string),
            root_out_dir: root_out.map(str::to_string),
        }
    }

    #[test]
    fn the_worker_nests_under_the_output_root_unless_declared_elsewhere() {
        let root = Path::new("/app");
        assert_eq!(
            worker_out_dir(&env(None, None), root, Path::new("/app/dist")),
            Path::new("/app/dist/my_worker")
        );
        assert_eq!(
            worker_out_dir(&env(None, None), root, Path::new("/out")),
            Path::new("/out/my_worker"),
            "--out moves the nested default along"
        );
        assert_eq!(
            worker_out_dir(&env(Some("worker-dist"), None), root, Path::new("/out")),
            Path::new("/app/worker-dist"),
            "a declared outDir outside the root stays"
        );
    }

    #[test]
    fn builtins_and_polyfills_are_recognized() {
        assert!(is_node_builtin("node:fs"));
        assert!(is_node_builtin("fs/promises"));
        assert!(!is_node_builtin("node:"));
        assert!(!is_node_builtin("react"));
        assert!(is_node_polyfill("unenv/runtime/node/fs"));
        assert!(is_node_polyfill("@cloudflare/unenv-preset/node/process"));
        assert!(is_start_default_entry(
            "/app/node_modules/@tanstack/react-start/dist/default-entry/server.js"
        ));
    }
}
