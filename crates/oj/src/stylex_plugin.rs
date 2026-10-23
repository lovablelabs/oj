//! oj's native `@stylexjs/unplugin` in rolldown bundles (`oj build`, Start):
//! the plugin's `enforce: 'pre'` transform, collecting every module's rules
//! for the stylesheet the build then emits.

use std::borrow::Cow;
use std::path::Path;
use std::sync::Arc;

use oj_stylex::plugin::StylexPlugin;
use rolldown_common::ModuleType;
use rolldown_plugin::__inner::{Pluginable, SharedPluginable};
use rolldown_plugin::{
    HookLoadArgs, HookLoadOutput, HookLoadReturn, HookResolveIdArgs, HookResolveIdOutput,
    HookResolveIdReturn, HookTransformArgs, HookTransformOutput, HookTransformOutputMap,
    HookTransformReturn, Plugin, PluginContext, PluginHookMeta, PluginOrder,
    SharedLoadPluginContext, SharedTransformPluginContext,
};

#[derive(Debug)]
struct StylexTransform {
    plugin: Arc<StylexPlugin>,
}

impl Plugin for StylexTransform {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed(oj_stylex::plugin::PLUGIN_NAME)
    }

    fn register_hook_usage(&self) -> rolldown_plugin::HookUsage {
        use rolldown_plugin::HookUsage;
        HookUsage::ResolveId | HookUsage::Load | HookUsage::Transform
    }

    fn resolve_id_meta(&self) -> Option<PluginHookMeta> {
        Some(PluginHookMeta {
            order: Some(PluginOrder::Pre),
        })
    }

    fn transform_meta(&self) -> Option<PluginHookMeta> {
        Some(PluginHookMeta {
            order: Some(PluginOrder::Pre),
        })
    }

    /// `virtual:stylex:runtime` / `virtual:stylex:css-only`, kept as the id.
    async fn resolve_id(
        &self,
        _ctx: &PluginContext,
        args: &HookResolveIdArgs<'_>,
    ) -> HookResolveIdReturn {
        Ok(self
            .plugin
            .virtual_module(args.specifier)
            .map(|_| HookResolveIdOutput::from_id(args.specifier)))
    }

    async fn load(&self, _ctx: SharedLoadPluginContext, args: &HookLoadArgs<'_>) -> HookLoadReturn {
        Ok(self
            .plugin
            .virtual_module(args.id)
            .map(|code| HookLoadOutput {
                code: code.into(),
                module_type: Some(ModuleType::Js),
                ..Default::default()
            }))
    }

    async fn transform(
        &self,
        _ctx: SharedTransformPluginContext,
        args: &HookTransformArgs<'_>,
    ) -> HookTransformReturn {
        let id = args.id;
        if id.starts_with('\0') || !self.plugin.handles(id, args.code) {
            return Ok(None);
        }
        let file = id.split('?').next().unwrap_or(id);
        let out = self
            .plugin
            .transform_source(Path::new(file), args.code)
            .map_err(|e| anyhow::anyhow!("{e}\n  in {file}"))?;
        let Some(out) = out else {
            self.plugin.set_module_rules(id, Vec::new());
            return Ok(None);
        };
        self.plugin.set_module_rules(id, out.rules);
        let map = out.map.and_then(|m| {
            oxc_sourcemap::SourceMap::from_json_string(&m)
                .ok()
                .map(|s| s.into_owned())
        });
        Ok(Some(HookTransformOutput {
            code: Some(out.code),
            map: match map {
                Some(map) => HookTransformOutputMap::Sourcemap(Box::new(map)),
                None => HookTransformOutputMap::Omitted,
            },
            ..Default::default()
        }))
    }
}

/// The transform as a bundle plugin.
pub(crate) fn transform(plugin: &Arc<StylexPlugin>) -> SharedPluginable {
    Pluginable::new_shared(StylexTransform {
        plugin: Arc::clone(plugin),
    })
}

/// The plugin's stylesheet as its build hooks write it: every collected rule,
/// through lightningcss for the app's production browserslist.
pub(crate) fn stylesheet(plugin: &StylexPlugin, root: &Path) -> anyhow::Result<Option<String>> {
    let Some(sheet) = plugin.stylesheet().map_err(|e| anyhow::anyhow!(e))? else {
        return Ok(None);
    };
    oj_css::process_for_browserslist("stylex.css", &sheet, root, "production")
        .map(Some)
        .map_err(|e| anyhow::anyhow!(e))
}
