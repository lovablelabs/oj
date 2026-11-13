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

/// The Start manifest of a production server bundle.
///
/// `tanstack-start-manifest:v` is answered by the app's own Start, whose
/// manifest plugins a build hosts: the routes' preloads and scripts built
/// from the client bundle, under Vite's id (`\0tanstack-start-manifest:v`),
/// so a plugin's `transform` sees the module it sees under `vite build`. That
/// manifest names no stylesheet: oj links the client's sheets itself and
/// settles their URLs after the client bundle is written (a plugin's late CSS
/// replaces a sheet). So the framework's import goes to a wrapper oj writes
/// (`manifest-sheets.ts`), which imports the manifest by the same specifier
/// and adds the sheets to the root route. With no Start plugin answering,
/// the wrapper's import ends at oj's own manifest through the alias.
#[derive(Debug)]
pub(crate) struct StartManifestSheets {
    pub wrapper: std::path::PathBuf,
}

pub(crate) const START_MANIFEST: &str = "tanstack-start-manifest:v";

impl StartManifestSheets {
    /// The wrapper module: `sheets` in link order, `client_entry` for the
    /// Start versions that read it at the top level (1.167 and earlier).
    pub fn module(sheets: &[String], client_entry: &str) -> String {
        format!(
            r#"import {{ tsrStartManifest as startManifest }} from "{START_MANIFEST}";
const sheets = {sheets};
const clientEntry = {entry};
const hrefOf = (sheet) => (typeof sheet === "string" ? sheet : sheet?.href);
export const tsrStartManifest = () => {{
  const manifest = startManifest();
  const routes = (manifest.routes ??= {{}});
  const root = (routes.__root__ ??= {{}});
  // The root links every sheet, in the order oj linked them; what Start
  // listed for a route (from the chunks' `importedCss`) is one of them, so
  // it goes from that route and stays with the root alone.
  const ours = new Set(sheets);
  const others = (list) => (list ?? []).filter((sheet) => !ours.has(hrefOf(sheet)));
  const otherAssets = (list) => (list ?? []).filter((asset) => !ours.has(asset?.attrs?.href));
  for (const [id, route] of Object.entries(routes)) {{
    if (id === "__root__" || !route) continue;
    if (route.css) route.css = others(route.css);
    if (route.assets) route.assets = otherAssets(route.assets);
  }}
  root.css = [...sheets, ...others(root.css)];
  root.assets = [
    ...otherAssets(root.assets),
    ...sheets.map((href) => ({{ tag: "link", attrs: {{ rel: "stylesheet", href }} }})),
  ];
  manifest.clientEntry ??= clientEntry;
  return manifest;
}};
"#,
            sheets = serde_json::json!(sheets),
            entry = serde_json::json!(client_entry),
        )
    }
}

impl Plugin for StartManifestSheets {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("oj:start-manifest-sheets")
    }

    fn register_hook_usage(&self) -> rolldown_plugin::HookUsage {
        rolldown_plugin::HookUsage::ResolveId
    }

    async fn resolve_id(
        &self,
        _ctx: &PluginContext,
        args: &HookResolveIdArgs<'_>,
    ) -> HookResolveIdReturn {
        if args.specifier != START_MANIFEST
            || args
                .importer
                .is_some_and(|from| Path::new(from) == self.wrapper)
        {
            return Ok(None);
        }
        Ok(Some(HookResolveIdOutput::from_id(
            self.wrapper.to_string_lossy().into_owned(),
        )))
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
    /// For a `require()`: `require` in place of `import` (getConditions).
    require: oj_resolver::OjResolver,
    native_after: bool,
}

impl TsconfigPathsPlugin {
    pub fn new(resolver: oj_resolver::OjResolver, native_after: bool) -> Self {
        TsconfigPathsPlugin {
            require: resolver.require_variant(),
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
        rolldown_plugin::HookUsage::ResolveId | rolldown_plugin::HookUsage::WatchChange
    }

    // A rebuild after a file appears or goes, or a JSON file (a tsconfig, a
    // package.json) changes, sees the files as they are now.
    async fn watch_change(
        &self,
        _ctx: &PluginContext,
        path: &str,
        event: rolldown_common::WatcherChangeKind,
    ) -> rolldown_plugin::HookNoopReturn {
        if path.ends_with(".json") || !matches!(event, rolldown_common::WatcherChangeKind::Update) {
            self.resolver.clear_cache();
            self.require.clear_cache();
        }
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
        let resolver = if args.kind == rolldown_common::ImportKind::Require {
            &self.require
        } else {
            &self.resolver
        };
        let answer = match resolver.tsconfig_answer(Path::new(file), spec, self.native_after) {
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
