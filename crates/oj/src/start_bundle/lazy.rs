// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Every `import()` of the hot client bundle goes through the page's module
//! registry instead of a chunk file name fixed when the page loaded, so an
//! open page importing a module for the first time runs its current code and
//! binds to the current instances of the modules it already has, as a page
//! under Vite's dev server does (its `import()` is a request the server
//! answers with the module as it is now).
//!
//! The mechanism is rolldown's: an `import()` whose target id ends in
//! `?rolldown-lazy=1` renders as `__rolldown_runtime__.requestLazy(id, proxy)`
//! and the dev engine compiles the target for the page that asks
//! (`DevEngine::compile_lazy_entry`). rolldown's own plugin for it
//! (rolldown_plugin_lazy_compilation) leaves the target out of the graph
//! until a page asks; this one keeps it in from the first build (the proxy
//! always imports its target), so the bundle's module closure, its style
//! sheets, the watched files and the hot patches are what they were.

use std::borrow::Cow;
use std::sync::Arc;

use arcstr::ArcStr;
use rolldown_common::{is_lazy_proxy, lazy_proxy_target, ImportKind, LAZY_PROXY_QUERY};
use rolldown_plugin::{
    HookLoadArgs, HookLoadOutput, HookLoadReturn, HookResolveIdArgs, HookResolveIdOutput,
    HookResolveIdReturn, HookUsage, Plugin, PluginContext, PluginContextResolveOptions,
    SharedLoadPluginContext,
};
use rolldown_utils::dashmap::FxDashMap;

#[derive(Debug, Default)]
pub(crate) struct LazyImports {
    /// Each proxy id and how its target resolved.
    proxies: FxDashMap<ArcStr, Target>,
}

#[derive(Debug)]
struct Target {
    side_effects: Option<rolldown_common::side_effects::HookSideEffects>,
    package_json_path: Option<String>,
}

impl LazyImports {
    fn proxy(id: ArcStr) -> HookResolveIdOutput {
        HookResolveIdOutput {
            id,
            external: None,
            normalize_external_id: None,
            side_effects: None,
            package_json_path: None,
            attributes: None,
        }
    }
}

impl Plugin for LazyImports {
    fn name(&self) -> Cow<'static, str> {
        "oj:start-lazy-imports".into()
    }

    fn register_hook_usage(&self) -> HookUsage {
        HookUsage::ResolveId | HookUsage::Load
    }

    async fn resolve_id(
        &self,
        ctx: &PluginContext,
        args: &HookResolveIdArgs<'_>,
    ) -> HookResolveIdReturn {
        if is_lazy_proxy(args.specifier) && self.proxies.contains_key(args.specifier) {
            return Ok(Some(Self::proxy(args.specifier.into())));
        }
        if !matches!(args.kind, ImportKind::DynamicImport) || !args.attributes.is_empty() {
            return Ok(None);
        }
        // The proxy's own `import()` of its target is the edge into the real
        // module: the id as it resolved the first time, never resolved again
        // (a virtual id is no specifier any plugin answers).
        if let Some(importer) = args.importer.filter(|i| is_lazy_proxy(i)) {
            if let Some(target) = self.proxies.get(importer) {
                if lazy_proxy_target(importer) == Some(args.specifier) {
                    return Ok(Some(HookResolveIdOutput {
                        side_effects: target.side_effects,
                        package_json_path: target.package_json_path.clone(),
                        ..Self::proxy(args.specifier.into())
                    }));
                }
                return Ok(None);
            }
        }
        // What the specifier does not resolve to is not this plugin's to
        // report: left to the resolution that follows, the import fails the
        // bundle as an unresolved import that names it and its importer, as
        // it does without this plugin.
        let Ok(Ok(target)) = ctx
            .resolve(
                args.specifier,
                args.importer,
                Some(PluginContextResolveOptions {
                    import_kind: ImportKind::DynamicImport,
                    is_entry: false,
                    skip_self: true,
                    custom: Arc::clone(&args.custom),
                    attributes: args.attributes.clone(),
                }),
            )
            .await
        else {
            return Ok(None);
        };
        // An external has no factory in the page's registry.
        if target.external.is_external() {
            return Ok(None);
        }
        let id: ArcStr = if is_lazy_proxy(target.id.as_str()) {
            target.id.as_str().into()
        } else {
            format!("{}{LAZY_PROXY_QUERY}", target.id).into()
        };
        self.proxies.insert(
            id.clone(),
            Target {
                side_effects: target.side_effects,
                package_json_path: target
                    .package_json
                    .as_ref()
                    .map(|p| p.realpath().to_string_lossy().into_owned()),
            },
        );
        Ok(Some(Self::proxy(id)))
    }

    async fn load(&self, _ctx: SharedLoadPluginContext, args: &HookLoadArgs<'_>) -> HookLoadReturn {
        let Some(target) = lazy_proxy_target(args.id) else {
            return Ok(None);
        };
        if !self.proxies.contains_key(args.id) {
            return Ok(None);
        }
        // Never executed: every `import()` of this id renders as a
        // `requestLazy` call.
        let code = format!("import({});\n", serde_json::to_string(target)?);
        Ok(Some(HookLoadOutput {
            code: ArcStr::from(code),
            module_type: Some(rolldown_common::ModuleType::Js),
            ..Default::default()
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Compiling a module for a page's `import()` leaves the engine with
    /// nothing to build: the module was in the graph already, so the output
    /// is still current and the next import or edit queues behind no build.
    #[tokio::test]
    async fn compiling_a_module_for_an_import_starts_no_build() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = std::fs::canonicalize(tmp.path()).unwrap();
        std::fs::write(
            dir.join("main.js"),
            "import('./one.js');\nimport('./two.js');\n",
        )
        .unwrap();
        std::fs::write(dir.join("one.js"), "export { dep } from './dep.js';\n").unwrap();
        std::fs::write(dir.join("two.js"), "export const two = 'second';\n").unwrap();
        std::fs::write(dir.join("dep.js"), "export const dep = 'shared';\n").unwrap();
        let proxy = |name: &str| format!("{}{LAZY_PROXY_QUERY}", dir.join(name).to_string_lossy());

        let builds = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&builds);
        let engine = rolldown_dev::DevEngine::new(
            rolldown::BundlerConfig::new(
                rolldown::BundlerOptions {
                    input: Some(vec!["./main.js".to_string().into()]),
                    cwd: Some(dir.clone()),
                    experimental: Some(rolldown::ExperimentalOptions {
                        dev_mode: Some(rolldown_common::DevModeOptions {
                            lazy: Some(false),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                vec![rolldown_plugin::__inner::Pluginable::new_shared(
                    LazyImports::default(),
                )],
            ),
            rolldown_dev::DevOptions {
                on_output: Some(Arc::new(move |_| {
                    counted.fetch_add(1, Ordering::SeqCst);
                })),
                watch: Some(rolldown_dev::DevWatchOptions {
                    enabled: Some(false),
                    skip_write: true,
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .unwrap();
        engine.run().await.unwrap();
        assert_eq!(builds.load(Ordering::SeqCst), 1, "the first build");

        for (name, client, has) in [
            ("one.js", "a", "shared"),
            ("two.js", "a", "second"),
            ("one.js", "b", "shared"),
        ] {
            engine.register_client(client.to_string()).await;
            let chunk = engine
                .compile_lazy_entry(proxy(name), client.to_string())
                .await
                .unwrap();
            assert!(chunk.code.contains(has), "{name}: {}", chunk.code);
        }
        // Whatever the compiles queued has run by the time this returns.
        engine.ensure_latest_bundle_output().await.unwrap();
        let state = engine.get_bundle_state().await.unwrap();
        engine.close().await.unwrap();
        assert!(!state.has_stale_output, "the output is still the graph's");
        assert_eq!(
            builds.load(Ordering::SeqCst),
            1,
            "a compile for an import started a build"
        );
    }
}
