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
        // A map the compiler wrote and nothing can read would break the
        // module's map chain without a word: fail the module instead.
        let map = out
            .map
            .map(|m| {
                oxc_sourcemap::SourceMap::from_json_string(&m)
                    .map(|s| s.into_owned())
                    .map_err(|e| {
                        anyhow::anyhow!("StyleX wrote an unreadable sourcemap for {file}: {e}")
                    })
            })
            .transpose()?;
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
    oj_css::process_for_browserslist(
        "stylex.css",
        &sheet,
        root,
        "production",
        plugin.lightningcss_options(),
    )
    .map(Some)
    .map_err(|e| anyhow::anyhow!(e))
}

/// The sidecar beside a cached Start client bundle: the rules its modules
/// compiled to, valid for that bundle's store key and this plugin's options.
const RULES_SIDECAR: &str = "stylex-rules.json";

/// Saves the rules of `modules`, the bundle cached under `key`: only its
/// own, so a restore brings back no other environment's (or a gone module's).
pub(crate) fn save_rules(plugin: &StylexPlugin, dir: &Path, key: &str, modules: &[String]) {
    let modules: std::collections::HashSet<&str> = modules.iter().map(String::as_str).collect();
    let rules: Vec<_> = plugin
        .module_rules()
        .into_iter()
        .filter(|(id, _)| modules.contains(id.as_str()))
        .collect();
    let sidecar = serde_json::json!({
        "key": key,
        "fingerprint": plugin.fingerprint(),
        "modules": rules,
    });
    let _ = std::fs::write(dir.join(RULES_SIDECAR), sidecar.to_string());
}

/// Restores the rules saved for the bundle cached under `key`; false when
/// none were saved for it (or for these options), and the bundle must be
/// rebuilt for its modules to compile.
pub(crate) fn restore_rules(plugin: &StylexPlugin, dir: &Path, key: &str) -> bool {
    let Some(sidecar) = std::fs::read(dir.join(RULES_SIDECAR))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
    else {
        return false;
    };
    let saved_for =
        |field: &str, want: &str| sidecar.get(field).and_then(|v| v.as_str()) == Some(want);
    if !saved_for("key", key) || !saved_for("fingerprint", plugin.fingerprint()) {
        return false;
    }
    let Some(modules) = sidecar.get("modules").cloned().and_then(|m| {
        serde_json::from_value::<Vec<(String, Vec<oj_stylex::rules::StylexRule>)>>(m).ok()
    }) else {
        return false;
    };
    for (id, rules) in modules {
        plugin.set_module_rules(&id, rules);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "import * as stylex from \"@stylexjs/stylex\";\n\
                       export const s = stylex.create({ a: { color: \"olive\" } });\n";

    fn plugin(options: serde_json::Value) -> StylexPlugin {
        StylexPlugin::new(&options, Path::new("/app"), false, true).unwrap()
    }

    #[test]
    fn saved_rules_restore_only_for_their_bundle_and_options() {
        let dir = std::env::temp_dir().join(format!("oj-stylex-sidecar-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cold = plugin(serde_json::json!({}));
        let rules = cold
            .transform_source(Path::new("/app/src/a.ts"), SRC)
            .unwrap()
            .unwrap()
            .rules;
        cold.set_module_rules("/app/src/a.ts", rules.clone());
        // Another environment's module, not the bundle's: never saved.
        cold.set_module_rules("/app/src/ssr-only.ts", rules);
        save_rules(&cold, &dir, "k1", &["/app/src/a.ts".to_string()]);

        let warm = plugin(serde_json::json!({}));
        assert!(!restore_rules(&warm, &dir, "k2"), "another bundle's key");
        assert_eq!(warm.stylesheet().unwrap(), None);
        assert!(restore_rules(&warm, &dir, "k1"));
        assert_eq!(
            warm.module_rules()
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            ["/app/src/a.ts"]
        );

        let other_options = plugin(serde_json::json!({ "classNamePrefix": "q" }));
        assert!(!restore_rules(&other_options, &dir, "k1"), "other options");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
