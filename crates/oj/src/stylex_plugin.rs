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

/// What a cached Start client bundle keeps beside its chunks: the rules its
/// modules compiled to, valid for that bundle and this plugin's options. It
/// lives with the generation in the bundle store, so another worktree that
/// restores the bundle gets its rules too.
const RULES_ATTACHMENT: &str = "stylex-rules.json";

/// A module id as the attachment stores it: in-root files `ws:<rel>` (ids
/// are the bundler's realpaths, so against the canonical root), anything
/// else as is. An out-of-root id is the same path in every worktree whose
/// bundle shares the generation, since the bundle key covers it.
fn stored_id(canonical_root: &Path, id: &str) -> String {
    match Path::new(id).strip_prefix(canonical_root) {
        Ok(rel) if !rel.as_os_str().is_empty() => format!("ws:{}", rel.to_string_lossy()),
        _ => id.to_string(),
    }
}

fn canonical(root: &Path) -> std::path::PathBuf {
    std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
}

/// Saves the rules of `modules`, the bundle cached under `key`: only its
/// own, so a restore brings back no other environment's (or a gone module's).
pub(crate) fn save_rules(
    plugin: &StylexPlugin,
    store: &oj_cache::start_bundle::StartBundleStore,
    root: &Path,
    key: &str,
    modules: &[String],
) {
    let root = canonical(root);
    let modules: std::collections::HashSet<&str> = modules.iter().map(String::as_str).collect();
    let rules: Vec<_> = plugin
        .module_rules()
        .into_iter()
        .filter(|(id, _)| modules.contains(id.as_str()))
        .map(|(id, rules)| (stored_id(&root, &id), rules))
        .collect();
    let saved = serde_json::json!({
        "fingerprint": plugin.fingerprint(),
        "modules": rules,
    });
    store.attach(key, RULES_ATTACHMENT, saved.to_string().as_bytes());
}

/// Restores the rules saved with the bundle cached under `key`, rehomed onto
/// this root; false when none were saved for it (or for these options), and
/// the bundle must be rebuilt for its modules to compile.
pub(crate) fn restore_rules(
    plugin: &StylexPlugin,
    store: &oj_cache::start_bundle::StartBundleStore,
    root: &Path,
    key: &str,
) -> bool {
    let Some(saved) = store
        .attachment(key, RULES_ATTACHMENT)
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
    else {
        return false;
    };
    if saved.get("fingerprint").and_then(|v| v.as_str()) != Some(plugin.fingerprint()) {
        return false;
    }
    let Some(modules) = saved.get("modules").cloned().and_then(|m| {
        serde_json::from_value::<Vec<(String, Vec<oj_stylex::rules::StylexRule>)>>(m).ok()
    }) else {
        return false;
    };
    let root = canonical(root);
    for (id, rules) in modules {
        let id = match id.strip_prefix("ws:") {
            Some(rel) => root.join(rel).to_string_lossy().into_owned(),
            None => id,
        };
        plugin.set_module_rules(&id, rules);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "import * as stylex from \"@stylexjs/stylex\";\n\
                       export const s = stylex.create({ a: { color: \"olive\" } });\n";

    /// A bundle store over `dir` holding one generation for `root`, and its key.
    fn store_with_generation(
        dir: &Path,
        root: &Path,
    ) -> (oj_cache::start_bundle::StartBundleStore, String) {
        let start = dir.join(format!(
            "start-{}",
            root.display().to_string().replace('/', "_")
        ));
        std::fs::create_dir_all(start.join("client-chunks")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.ts"), SRC).unwrap();
        std::fs::write(
            start.join("closure.json"),
            serde_json::to_vec(&[root.join("src/a.ts")]).unwrap(),
        )
        .unwrap();
        std::fs::write(start.join("client-chunks/entry.js"), "x").unwrap();
        std::fs::write(
            start.join("client-chunks.json"),
            r#"{"entry":"entry.js","files":[{"name":"entry.js","size":1}]}"#,
        )
        .unwrap();
        for name in [
            "client-entry.modules",
            "manifest.ts",
            "glob-scopes.json",
            "css-urls.json",
        ] {
            std::fs::write(
                start.join(name),
                if name.ends_with(".json") { "[]" } else { "" },
            )
            .unwrap();
        }
        let store = oj_cache::start_bundle::StartBundleStore::at_dirs(
            root,
            dir.join("shared"),
            root.join(".oj-cache/start-bundle"),
            8,
            "0.0.1-test",
            oj_cache::integrity::VerifyMode::Standard,
            "development",
        );
        let (key, _) = store.persist(&start).unwrap();
        (store, key)
    }

    // The rules travel with the generation: another worktree restoring the
    // same bundle gets them under its own module ids.
    #[test]
    fn saved_rules_restore_in_another_worktree_under_its_ids() {
        let dir = std::env::temp_dir().join(format!("oj-stylex-share-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (a, b) = (dir.join("wt-a"), dir.join("wt-b"));
        let (store_a, key) = store_with_generation(&dir, &a);
        let (store_b, key_b) = store_with_generation(&dir, &b);
        assert_eq!(key, key_b, "same content, one generation");
        let id = |root: &Path| {
            canonical(root)
                .join("src/a.ts")
                .to_string_lossy()
                .into_owned()
        };
        let cold = StylexPlugin::new(&serde_json::json!({}), &a, false, true).unwrap();
        let rules = cold
            .transform_source(Path::new(&id(&a)), SRC)
            .unwrap()
            .unwrap()
            .rules;
        cold.set_module_rules(&id(&a), rules);
        save_rules(&cold, &store_a, &a, &key, &[id(&a)]);

        let warm = StylexPlugin::new(&serde_json::json!({}), &b, false, true).unwrap();
        assert_eq!(
            warm.fingerprint(),
            cold.fingerprint(),
            "one fingerprint per app"
        );
        assert!(restore_rules(&warm, &store_b, &b, &key));
        assert_eq!(
            warm.module_rules()
                .iter()
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>(),
            [id(&b)]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saved_rules_restore_only_for_their_bundle_and_options() {
        let dir = std::env::temp_dir().join(format!("oj-stylex-sidecar-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let app = dir.join("app");
        let (store, key) = store_with_generation(&dir, &app);
        let id = |rel: &str| canonical(&app).join(rel).to_string_lossy().into_owned();
        let cold = StylexPlugin::new(&serde_json::json!({}), &app, false, true).unwrap();
        let rules = cold
            .transform_source(Path::new(&id("src/a.ts")), SRC)
            .unwrap()
            .unwrap()
            .rules;
        cold.set_module_rules(&id("src/a.ts"), rules.clone());
        // Another environment's module, not the bundle's: never saved.
        cold.set_module_rules(&id("src/ssr-only.ts"), rules);
        save_rules(&cold, &store, &app, &key, &[id("src/a.ts")]);

        let warm = StylexPlugin::new(&serde_json::json!({}), &app, false, true).unwrap();
        assert!(
            !restore_rules(&warm, &store, &app, "0000"),
            "another bundle's key"
        );
        assert_eq!(warm.stylesheet().unwrap(), None);
        assert!(restore_rules(&warm, &store, &app, &key));
        assert_eq!(
            warm.module_rules()
                .iter()
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>(),
            [id("src/a.ts")]
        );

        let other_options = StylexPlugin::new(
            &serde_json::json!({ "classNamePrefix": "q" }),
            &app,
            false,
            true,
        )
        .unwrap();
        assert!(
            !restore_rules(&other_options, &store, &app, &key),
            "other options"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
