// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Vite's verdict on a module's side effects for the imports rolldown's own
//! resolver answers (the plugin host passed on them, or there is none): the
//! package's `sideEffects` field, as `moduleSideEffects`. rolldown takes a
//! `true` as "analyze the module" and a `false` as none, so a pure module a
//! glob covers (a StyleX tokens file under `**/*.stylex.ts`) still drops out
//! of a chunk that imports it for nothing, as under Vite's own resolver.
//! Left to rolldown, the field is read as the verdict itself, and every
//! chunk kept an `import "./conditions.stylex-*.js"` (Lovable's server
//! bundle: 64 of them).

use std::borrow::Cow;
use std::path::Path;

use rolldown_common::side_effects::HookSideEffects;
use rolldown_plugin::{
    HookResolveIdArgs, HookResolveIdOutput, HookResolveIdReturn, Plugin, PluginContext,
    PluginContextResolveOptions,
};

/// Last in a bundle's plugins: what reaches it, no plugin answered.
#[derive(Debug)]
pub(crate) struct ViteSideEffects;

impl Plugin for ViteSideEffects {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("oj:vite-side-effects")
    }

    fn register_hook_usage(&self) -> rolldown_plugin::HookUsage {
        rolldown_plugin::HookUsage::ResolveId
    }

    async fn resolve_id(
        &self,
        ctx: &PluginContext,
        args: &HookResolveIdArgs<'_>,
    ) -> HookResolveIdReturn {
        let Ok(found) = ctx
            .resolve(
                args.specifier,
                args.importer,
                Some(PluginContextResolveOptions {
                    import_kind: args.kind,
                    is_entry: args.is_entry,
                    skip_self: true,
                    custom: std::sync::Arc::clone(&args.custom),
                    attributes: args.attributes.clone(),
                }),
            )
            .await?
        else {
            return Ok(None);
        };
        if found.external.is_external() || found.side_effects.is_some() {
            return Ok(None);
        }
        let Some(verdict) = found.package_json.as_ref().and_then(|p| {
            let relative = Path::new(found.id.as_str())
                .strip_prefix(p.realpath().parent()?)
                .unwrap_or(Path::new(found.id.as_str()));
            p.check_side_effects_for(&relative.to_string_lossy())
        }) else {
            return Ok(None);
        };
        let mut out = HookResolveIdOutput::from_resolved_id(found);
        out.side_effects = Some(if verdict {
            HookSideEffects::True
        } else {
            HookSideEffects::False
        });
        Ok(Some(out))
    }
}
