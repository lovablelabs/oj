// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Start's import protection in the dev client bundle, where oj checks
//! imports itself (see `oj_server::import_protection`). It sits where
//! Start's plugin does in Vite's order: after the aliases, ahead of the
//! app's other plugins, resolving through them and the bundler.

use std::borrow::Cow;
use std::sync::Arc;

use oj_server::import_protection::{self, Before, ImportProtection, Side};
use rolldown_common::ModuleType;
use rolldown_plugin::{
    HookLoadArgs, HookLoadOutput, HookLoadReturn, HookResolveIdArgs, HookResolveIdOutput,
    HookResolveIdReturn, HookTransformArgs, HookTransformOutput, HookTransformReturn, HookUsage,
    Plugin, PluginContext, PluginContextResolveOptions, SharedLoadPluginContext,
    SharedTransformPluginContext,
};

#[derive(Debug)]
pub(crate) struct ImportProtectionPlugin(pub Arc<ImportProtection>);

/// A violation as the bundle's error. The prefix is the one Vite puts on an
/// error Start's plugin throws, which tools reading the log match on.
pub(crate) fn violation(message: String) -> anyhow::Error {
    anyhow::anyhow!("Error: [plugin:tanstack-start-core:import-protection] {message}")
}

impl Plugin for ImportProtectionPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("oj:start-import-protection")
    }

    fn register_hook_usage(&self) -> HookUsage {
        HookUsage::ResolveId | HookUsage::Load | HookUsage::Transform | HookUsage::WatchChange
    }

    async fn watch_change(
        &self,
        _ctx: &PluginContext,
        path: &str,
        _event: rolldown_common::WatcherChangeKind,
    ) -> rolldown_plugin::HookNoopReturn {
        self.0.changed(Side::Client, path);
        Ok(())
    }

    async fn resolve_id(
        &self,
        ctx: &PluginContext,
        args: &HookResolveIdArgs<'_>,
    ) -> HookResolveIdReturn {
        if let Some(id) = import_protection::resolve_virtual(args.specifier) {
            return Ok(Some(HookResolveIdOutput::from_id(id)));
        }
        let Some(importer) = args.importer else {
            self.0.add_entry(Side::Client, args.specifier);
            return Ok(None);
        };
        match self
            .0
            .before_resolve(Side::Client, importer, args.specifier)
        {
            Before::Pass => Ok(None),
            Before::Virtual(id) => Ok(Some(HookResolveIdOutput::from_id(id))),
            Before::Denied(message) => Err(violation(message)),
            Before::Check => {
                let resolved = ctx
                    .resolve(
                        args.specifier,
                        Some(importer),
                        Some(PluginContextResolveOptions {
                            import_kind: args.kind,
                            is_entry: args.is_entry,
                            skip_self: true,
                            custom: Arc::clone(&args.custom),
                            attributes: args.attributes.clone(),
                        }),
                    )
                    .await?;
                // What nothing resolves is the bundle's own error to report.
                let Ok(found) = resolved else {
                    return Ok(None);
                };
                if !found.external.is_external() {
                    self.0
                        .after_resolve(Side::Client, importer, args.specifier, &found.id)
                        .map_err(violation)?;
                }
                Ok(Some(HookResolveIdOutput::from_resolved_id(found)))
            }
        }
    }

    async fn load(&self, _ctx: SharedLoadPluginContext, args: &HookLoadArgs<'_>) -> HookLoadReturn {
        Ok(
            import_protection::load_virtual(args.id).map(|code| HookLoadOutput {
                code: code.into(),
                module_type: Some(ModuleType::Js),
                ..Default::default()
            }),
        )
    }

    async fn transform(
        &self,
        _ctx: SharedTransformPluginContext,
        args: &HookTransformArgs<'_>,
    ) -> HookTransformReturn {
        Ok(self
            .0
            .self_denial(Side::Client, args.id, args.code)
            .map(|code| HookTransformOutput {
                code: Some(code),
                ..Default::default()
            }))
    }
}
