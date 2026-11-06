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

/// tsconfig `paths` as the app's Vite applies them, where vite:resolve
/// sits, ahead of rolldown's own resolver, which applies them natively
/// (oxc's order, ahead of packages):
/// `oj_resolver::OjResolver::tsconfig_answer`. `native_after` lets that
/// native resolution take what Vite's resolve plugin leaves, as `vite build`
/// does (a dev server never does).
pub(crate) struct TsconfigPathsPlugin {
    resolver: oj_resolver::OjResolver,
    native_after: bool,
}

impl TsconfigPathsPlugin {
    pub fn new(resolver: oj_resolver::OjResolver, native_after: bool) -> Self {
        TsconfigPathsPlugin {
            resolver,
            native_after,
        }
    }
}

impl std::fmt::Debug for TsconfigPathsPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TsconfigPathsPlugin")
    }
}

impl Plugin for TsconfigPathsPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("oj:tsconfig-paths")
    }

    fn register_hook_usage(&self) -> rolldown_plugin::HookUsage {
        rolldown_plugin::HookUsage::ResolveId | rolldown_plugin::HookUsage::BuildStart
    }

    // Each rebuild sees the files as they are now (a tsconfig edited, a
    // package installed since).
    async fn build_start(
        &self,
        _ctx: &PluginContext,
        _args: &rolldown_plugin::HookBuildStartArgs<'_>,
    ) -> rolldown_plugin::HookNoopReturn {
        self.resolver.clear_cache();
        Ok(())
    }

    async fn resolve_id(
        &self,
        ctx: &PluginContext,
        args: &HookResolveIdArgs<'_>,
    ) -> HookResolveIdReturn {
        let spec = args.specifier;
        let Some(importer) = args.importer else {
            return Ok(None);
        };
        if spec.starts_with('.')
            || spec.starts_with('/')
            || spec.contains('\0')
            || spec.contains(':')
        {
            return Ok(None);
        }
        let file = importer.split(['?', '#']).next().unwrap_or(importer);
        let answer = match self
            .resolver
            .tsconfig_answer(Path::new(file), spec, self.native_after)
        {
            oj_resolver::TsconfigAnswer::File(answer) => answer,
            oj_resolver::TsconfigAnswer::Bundler => return Ok(None),
            // Vite fails the import; so does this bundle.
            oj_resolver::TsconfigAnswer::Unresolved => {
                return Err(anyhow::anyhow!(
                    "Failed to resolve import \"{spec}\" from \"{file}\". Does the file exist?"
                ))
            }
        };
        let resolved = ctx
            .resolve(
                &answer.to_string_lossy(),
                args.importer,
                Some(PluginContextResolveOptions {
                    import_kind: args.kind,
                    is_entry: args.is_entry,
                    skip_self: true,
                    ..Default::default()
                }),
            )
            .await?;
        Ok(resolved.ok().map(HookResolveIdOutput::from_resolved_id))
    }
}
