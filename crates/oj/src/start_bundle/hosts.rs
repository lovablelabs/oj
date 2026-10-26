// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Where a Start bundle runs the app's Vite plugins: a build's own host,
//! shared by its environments, or a container of the dev server's host for
//! the dev client bundle, in Start bundle mode either way (the framework
//! plugins oj reimplements left out, a plugin's `this.resolve` ending in the
//! bundle's own resolution).

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
    // The hook gates read the plugins' filters before any module is offered,
    // as rolldown reads them in Vite: a filtered hook never crosses into JS
    // for a module it does not claim.
    host.build_hook_plan().await;
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

/// The dev server's client bundle on the dev server's own plugin host: a
/// container of its client environment (one buildStart, configureServer run
/// once, by the server), the framework plugins oj reimplements left out.
pub(crate) async fn open(dev: &PluginHost, env_name: &str) -> anyhow::Result<BundleHost> {
    let host = dev
        .open_environment_container(env_name, true, true)
        .await
        .map_err(|e| anyhow::anyhow!("the {env_name} plugin container failed to open: {e}"))?;
    host.build_hook_plan().await;
    let ctx = serve_bundler_resolve(&host);
    let (facts, defines, env_delta) = read_facts(&host, env_name).await?;
    Ok(BundleHost {
        host,
        ctx,
        facts,
        defines,
        env_delta,
    })
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
        if let Some(runtime) = host.runtime().cloned() {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// An app whose plugins are `plugins` (an `oj.plugins.mjs` body). Its root
    /// is cleaned before use and outlives the test: an in-process host moves
    /// the whole test process into it. Earlier runs' roots are swept a day on.
    fn app(name: &str, plugins: &str) -> PathBuf {
        let tmp = std::env::temp_dir().canonicalize().unwrap();
        let prefix = format!("oj-start-hosts-{name}-");
        for entry in std::fs::read_dir(&tmp).into_iter().flatten().flatten() {
            let stale = entry.file_name().to_string_lossy().starts_with(&prefix)
                && entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age > std::time::Duration::from_secs(24 * 60 * 60));
            if stale {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
        let root = tmp.join(format!("{prefix}{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("oj.plugins.mjs"), plugins).unwrap();
        root
    }

    /// [`app`] with the start-app fixture's `node_modules`, so the host strips
    /// TS/JSX with the app's own Vite; None (after saying why) without it.
    fn app_with_vite(name: &str, plugins: &str) -> Option<PathBuf> {
        let modules = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../e2e/fixtures/start-app/node_modules")
            .canonicalize()
            .ok()
            .filter(|m| m.join("vite").is_dir());
        let Some(modules) = modules else {
            assert!(
                std::env::var_os("OJ_REQUIRE_FIXTURES").is_none(),
                "OJ_REQUIRE_FIXTURES is set but the start-app fixture is not installed"
            );
            eprintln!("skipping: npm ci --prefix e2e/fixtures/start-app");
            return None;
        };
        let root = app(name, plugins);
        std::os::unix::fs::symlink(modules, root.join("node_modules")).unwrap();
        Some(root)
    }

    async fn client_host(root: &Path) -> BundleHost {
        spawn(
            &crate::start_bundle::dev::tests::start_at(root),
            None,
            "client",
        )
        .await
        .expect("the bundle host spawns")
        .expect("the app has plugins")
    }

    const FILTERED: &str = r#"
  { name: "md", transform: { filter: { id: /\.md$/ }, handler(code) { return code; } } },
  { name: "svg", load: { filter: { id: /\.bench-svg$/ }, handler() { return "export default 1"; } } },
  { name: "icons", resolveId: { filter: { id: /^bench-icons:/ }, handler(id) { return "\0" + id; } } },
"#;

    // Vite's plugin container (and rolldown, for a bundle) skips a hook whose
    // filter rejects the module: the Start bundle's gates do the same from the
    // first module on, so an ordinary module never crosses into JS.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_start_bundle_host_gates_each_hook_by_the_plugins_filters() {
        let root = app("filters", &format!("export default [{FILTERED}];\n"));
        let host = client_host(&root).await.host;
        let tsx = root.join("src/a.tsx").display().to_string();
        let md = root.join("src/a.md").display().to_string();
        assert!(!host.hook_wants_transform(&tsx, "export default 1"));
        assert!(host.hook_wants_transform(&md, "# a"));
        assert!(!host.hook_wants_load(&tsx));
        assert!(host.hook_wants_load(&root.join("src/a.bench-svg").display().to_string()));
        assert!(!host.hook_wants_resolve_id("react"));
        assert!(host.hook_wants_resolve_id("bench-icons:star"));
    }

    // The server build runs on the same plugin instances, minus those whose
    // applyToEnvironment drops them there: the gates follow the server set.
    #[tokio::test(flavor = "multi_thread")]
    async fn switching_environment_gates_by_the_next_environments_plugins() {
        let root = app(
            "switch",
            &format!(
                r#"export default [{FILTERED}
  {{ name: "client-only", applyToEnvironment: (env) => env.name === "client", transform(code) {{ return code; }} }},
];
"#
            ),
        );
        let client = client_host(&root).await;
        let tsx = root.join("src/a.tsx").display().to_string();
        assert!(
            client.host.hook_wants_transform(&tsx, "export default 1"),
            "the client's unfiltered transform takes every module"
        );
        let server = client.switch_environment("ssr").await.expect("switched");
        assert!(
            !server.host.hook_wants_transform(&tsx, "export default 1"),
            "the server set has only the filtered transform"
        );
        assert!(server
            .host
            .hook_wants_transform(&root.join("src/a.md").display().to_string(), "# a"));
    }

    // A worker bundle runs `worker.plugins`, a set of its own (Vite's
    // bundleWorkerEntry): its gates are that set's filters, not the app's.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_worker_container_gates_by_worker_plugins_filters() {
        let root = app(
            "worker",
            r#"export default [
  { name: "app-all", transform(code) { return code; } },
  { name: "worker-plugins", config() { return { worker: { plugins: () => [
    { name: "worker-md", transform: { filter: { id: /\.md$/ }, handler(code) { return code; } } },
  ] } }; } },
];
"#,
        );
        let client = client_host(&root).await;
        let tsx = root.join("src/a.tsx").display().to_string();
        assert!(client.host.hook_wants_transform(&tsx, "export default 1"));
        let worker = client
            .worker(&[root.join("src/w.ts")])
            .await
            .expect("a worker container");
        assert!(
            !worker.host.hook_wants_transform(&tsx, "export default 1"),
            "worker.plugins only claims .md"
        );
        assert!(worker
            .host
            .hook_wants_transform(&root.join("src/a.md").display().to_string(), "# a"));
        worker.close_worker().await.expect("closed");
    }

    // A normal transform's `filter.code` is tested on the code after Vite's
    // TS/JSX strip, which the gate never sees: a filter only compiled JSX
    // matches must still be offered the raw `.tsx`.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_code_filter_after_the_strip_never_gates_on_raw_code() {
        let Some(root) = app_with_vite(
            "code-after-strip",
            r#"export default [
  { name: "jsx-runtime", transform: { filter: { code: "jsx-dev-runtime" }, handler(code) { return code; } } },
];
"#,
        ) else {
            return;
        };
        let host = client_host(&root).await.host;
        let tsx = root.join("src/a.tsx").display().to_string();
        assert!(host.hook_wants_transform(&tsx, "export const A = () => <div />;"));
    }

    // A pre transform runs before the strip, on the code the gate sees: its
    // `filter.code` still gates.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_pre_transforms_code_filter_still_gates() {
        let Some(root) = app_with_vite(
            "code-pre",
            r#"export default [
  { name: "needle", enforce: "pre", transform: { filter: { code: "needle" }, handler(code) { return code; } } },
];
"#,
        ) else {
            return;
        };
        let host = client_host(&root).await.host;
        let ts = root.join("src/a.ts").display().to_string();
        assert!(!host.hook_wants_transform(&ts, "export const a = 1;"));
        assert!(host.hook_wants_transform(&ts, "export const needle = 1;"));
    }
}
