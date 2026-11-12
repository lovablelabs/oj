// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The plugin context's shape follows the app's Vite major. Vite 6 added the
//! Environment API: `this.environment` on the plugin context (Vite's
//! server/pluginContainer.ts EnvironmentPluginContainer) and the `hotUpdate`,
//! `configEnvironment` and `applyToEnvironment` hooks; Vite 5's PluginContext
//! is `meta`, `ssr` and the rollup members. A plugin that supports both lines
//! (`@tailwindcss/vite`) picks its code path by `this.environment`: present,
//! it calls `createIdResolver` off the app's `vite`, which Vite 5 does not
//! export; so a Vite 5 app's plugins must see the Vite 5 shape.

use super::*;

/// A host over `plugins_src` in a fresh root whose `node_modules/vite`
/// reports `vite_version` (`None`: no Vite installed, the line oj tracks).
async fn host_under_vite(
    tag: &str,
    vite_version: Option<&str>,
    plugins_src: &str,
) -> (PathBuf, std::sync::Arc<PluginHost>) {
    let root = std::env::temp_dir().join(format!("oj-plugin-ctx-{tag}-{}", std::process::id()));
    crate::plugins::test_roots::remove_root(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("package.json"),
        r#"{"name":"app","type":"module"}"#,
    )
    .unwrap();
    if let Some(version) = vite_version {
        // What plugin-host.mjs reads off the app's Vite: `version` from the
        // package Node's `import("vite")` resolves from the root.
        let vite = root.join("node_modules").join("vite");
        std::fs::create_dir_all(&vite).unwrap();
        std::fs::write(
            vite.join("package.json"),
            format!(r#"{{"name":"vite","version":"{version}","type":"module","main":"index.js"}}"#),
        )
        .unwrap();
        std::fs::write(
            vite.join("index.js"),
            format!(
                "export const version = \"{version}\";\nexport const rollupVersion = \"4.0.0\";\n"
            ),
        )
        .unwrap();
    }
    let plugins = root.join("oj.plugins.mjs");
    std::fs::write(&plugins, plugins_src).unwrap();
    let config = serde_json::json!({
        "config": { "root": root.display().to_string() },
        "env": { "command": "serve", "mode": "development" },
    })
    .to_string();
    let host = PluginHost::spawn(&root, &plugins, &config, None)
        .await
        .expect("the embedded engine spawns");
    (root, host)
}

/// A transform that reports the context it ran with: whether `environment`
/// is a member at all, its name, and the Vite version the meta carries.
const PROBE_PLUGIN: &str = r#"export default [{
  name: "probe",
  transform(code, id) {
    if (!id.endsWith("probe.js")) return null;
    return "/*" + JSON.stringify({
      hasEnvironment: "environment" in this,
      environment: this.environment === undefined ? null : this.environment.name,
      viteVersion: this.meta.viteVersion,
    }) + "*/";
  },
}];
"#;

async fn probe(tag: &str, vite_version: Option<&str>) -> serde_json::Value {
    let (root, host) = host_under_vite(tag, vite_version, PROBE_PLUGIN).await;
    let id = root.join("probe.js").display().to_string();
    let out = host
        .transform("export const x = 1;\n", &id, "")
        .await
        .expect("the probe transform runs");
    host.shutdown();
    crate::plugins::test_roots::remove_root(&root);
    let json = out
        .code
        .trim()
        .trim_start_matches("/*")
        .trim_end_matches("*/")
        .to_owned();
    serde_json::from_str(&json).unwrap_or_else(|e| panic!("probe output {json:?}: {e}"))
}

// Vite 5's plugin context has no `environment`, so a plugin that
// feature-detects Vite 6 on it takes its Vite 5 path.
#[tokio::test]
async fn under_vite_5_the_plugin_context_has_no_environment() {
    let seen = probe("v5", Some("5.4.21")).await;
    assert_eq!(
        seen["viteVersion"], "5.4.21",
        "the meta reports the app's Vite"
    );
    assert_eq!(
        seen["hasEnvironment"], false,
        "Vite 5's PluginContext has no `environment` member: {seen}"
    );
    assert_eq!(seen["environment"], serde_json::Value::Null);
}

// Vite 6+ and the Vite line oj tracks keep the Environment API shape.
#[tokio::test]
async fn under_vite_6_and_without_an_app_vite_the_context_carries_the_environment() {
    let seen = probe("v6", Some("6.3.5")).await;
    assert_eq!(seen["viteVersion"], "6.3.5");
    assert_eq!(seen["hasEnvironment"], true, "{seen}");
    assert_eq!(seen["environment"], "client");

    let seen = probe("none", None).await;
    assert_eq!(
        seen["hasEnvironment"], true,
        "no app Vite: the line oj tracks, {seen}"
    );
    assert_eq!(seen["environment"], "client");
}
