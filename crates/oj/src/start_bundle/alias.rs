// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The app's `resolve.alias` as Vite applies it (plugins/index.ts): ahead of
//! every app plugin and the core resolver, the first matching entry rewrites
//! the specifier, and the rewritten id resolves through the rest of the
//! pipeline. A rewritten id nothing resolves stays the answer, as in
//! rolldown's viteAliasPlugin: the import then fails to load, loudly, instead
//! of reaching the later resolvers as the original bare `@/x` and shipping to
//! the browser unresolved.

use std::borrow::Cow;
use std::path::Path;

use rolldown_plugin::{
    HookResolveIdArgs, HookResolveIdOutput, HookResolveIdReturn, Plugin, PluginContext,
    PluginContextResolveOptions,
};

use super::facts::AliasEntry;

#[derive(Debug)]
pub(crate) struct AppAliasPlugin {
    pub entries: Vec<AliasEntry>,
}

impl Plugin for AppAliasPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("oj:app-alias")
    }

    fn register_hook_usage(&self) -> rolldown_plugin::HookUsage {
        rolldown_plugin::HookUsage::ResolveId
    }

    async fn resolve_id(
        &self,
        ctx: &PluginContext,
        args: &HookResolveIdArgs<'_>,
    ) -> HookResolveIdReturn {
        let Some(updated) = self.entries.iter().find_map(|e| e.apply(args.specifier)) else {
            return Ok(None);
        };
        let resolved = ctx
            .resolve(
                &updated,
                args.importer,
                Some(PluginContextResolveOptions {
                    import_kind: args.kind,
                    is_entry: args.is_entry,
                    skip_self: true,
                    ..Default::default()
                }),
            )
            .await?;
        if let Ok(found) = resolved {
            return Ok(Some(HookResolveIdOutput::from_resolved_id(found)));
        }
        if !Path::new(&updated).is_absolute() {
            eprintln!(
                "oj: rewrote {} to {updated} but was not an absolute path and was not handled by other plugins. This will lead to duplicated modules for the same path. To avoid duplicating modules, you should resolve to an absolute path.",
                args.specifier
            );
        }
        Ok(Some(HookResolveIdOutput::from_id(updated)))
    }
}
