// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The version meta of the plugin context is the app's Vite's
//! (`basePluginContextMeta` in Vite's server/pluginContainer.ts). Only a
//! rolldown-based Vite (8) has `rolldownVersion`, and exports
//! `transformWithOxc` beside it; plugins pick between that and
//! `transformWithEsbuild` by `this.meta.rolldownVersion`, so the key must not
//! be there when the app's `vite` has no `transformWithOxc` to go with it.

use super::*;

/// A root whose `node_modules/vite` is `version`, exporting
/// `rolldownVersion` and `transformWithOxc` when `rolldown` is set, as Vite 8
/// does, and `transformWithEsbuild` always.
fn app_with_vite(tag: &str, version: &str, rolldown: bool) -> PathBuf {
    let root = std::env::temp_dir().join(format!("oj-plugin-meta-{tag}-{}", std::process::id()));
    crate::plugins::test_roots::remove_root(&root);
    let vite = root.join("node_modules").join("vite");
    std::fs::create_dir_all(&vite).unwrap();
    std::fs::write(
        root.join("package.json"),
        r#"{"name":"app","type":"module"}"#,
    )
    .unwrap();
    std::fs::write(
        vite.join("package.json"),
        format!(r#"{{"name":"vite","version":"{version}","type":"module","exports":{{".":"./index.js"}}}}"#),
    )
    .unwrap();
    let oxc = if rolldown {
        "export const rolldownVersion = \"1.2.19\";\n\
         export async function transformWithOxc(code) { return { code: \"/*oxc*/\" + code, map: null }; }\n"
    } else {
        ""
    };
    std::fs::write(
        vite.join("index.js"),
        format!(
            "export const version = \"{version}\";\nexport const rollupVersion = \"4.0.0\";\n{oxc}\
             export async function transformWithEsbuild(code) {{ return {{ code: \"/*esbuild*/\" + code, map: null }}; }}\n"
        ),
    )
    .unwrap();
    root
}

/// vite-plugin-svgr's `load`: `transformWithOxc` off `vite` when the meta
/// has `rolldownVersion`, else `transformWithEsbuild`; plus what the meta of
/// `config` (the minimal context) and `load` carry.
const SVGR_LIKE: &str = r#"let configMeta;
export default [{
  name: "svgr-like",
  enforce: "pre",
  config() { configMeta = { ...this.meta }; },
  async load(id) {
    if (!id.endsWith("logo.svg?react")) return;
    const vite = await import("vite");
    const code = "export default 1;";
    const out = this.meta.rolldownVersion != null
      ? await vite.transformWithOxc(code, id, { lang: "jsx" })
      : await vite.transformWithEsbuild(code, id, { loader: "jsx" });
    return out.code + "\n//" + JSON.stringify({
      config: "rolldownVersion" in configMeta,
      load: "rolldownVersion" in this.meta,
      rolldownVersion: this.meta.rolldownVersion ?? null,
    });
  },
}];
"#;

/// Whether `config` (the minimal context) sees a `viteVersion` key, and the
/// `viteVersion` `load` sees.
const VITE_VERSION_PROBE: &str = r#"let configMeta;
export default [{
  name: "vite-version-probe",
  config() { configMeta = { ...this.meta }; },
  load(id) {
    if (!id.endsWith("logo.svg?react")) return;
    return JSON.stringify({ config: "viteVersion" in configMeta, load: this.meta.viteVersion ?? null });
  },
}];
"#;

async fn load_under(tag: &str, version: &str, rolldown: bool) -> String {
    load_with(tag, version, rolldown, SVGR_LIKE).await
}

async fn load_with(tag: &str, version: &str, rolldown: bool, source: &str) -> String {
    let root = app_with_vite(tag, version, rolldown);
    let plugins = root.join("oj.plugins.mjs");
    std::fs::write(&plugins, source).unwrap();
    let config = serde_json::json!({
        "config": { "root": root.display().to_string() },
        "env": { "command": "serve", "mode": "development" },
    })
    .to_string();
    let host = PluginHost::spawn(&root, &plugins, &config, None)
        .await
        .expect("the embedded engine spawns");
    let id = root.join("src/logo.svg?react").display().to_string();
    let out = host.load(&id).await;
    host.shutdown();
    crate::plugins::test_roots::remove_root(&root);
    out.unwrap_or_else(|e| panic!("the load under vite {version} fails: {e}"))
        .expect("the plugin loads the svg")
}

// Vite 5 to 7 are not rolldown-based: no `rolldownVersion` in any meta, so
// the plugin takes `transformWithEsbuild`, which their `vite` exports.
#[tokio::test]
async fn under_vite_7_and_older_the_meta_has_no_rolldown_version() {
    for version in ["7.3.1", "6.4.1", "5.4.21"] {
        let out = load_under(&format!("v{version}"), version, false).await;
        assert_eq!(
            out,
            "/*esbuild*/export default 1;\n//{\"config\":false,\"load\":false,\"rolldownVersion\":null}",
            "under vite {version}"
        );
    }
}

// Vite 8 has it, from the app's `vite`, and the plugin's `transformWithOxc`.
#[tokio::test]
async fn under_vite_8_the_meta_has_the_app_rolldown_version() {
    let out = load_under("v8", "8.0.3", true).await;
    assert_eq!(
        out,
        "/*oxc*/export default 1;\n//{\"config\":true,\"load\":true,\"rolldownVersion\":\"1.2.19\"}"
    );
}

// `viteVersion` joined the meta in Vite 7 (`basePluginContextMeta`); Vite 6
// and 5 have no such key, in any hook.
#[tokio::test]
async fn under_vite_6_and_older_the_meta_has_no_vite_version() {
    for (version, rolldown, has_key) in [
        ("8.0.3", true, true),
        ("7.3.1", false, true),
        ("6.4.1", false, false),
        ("5.4.21", false, false),
    ] {
        let out = load_with(
            &format!("vv{version}"),
            version,
            rolldown,
            VITE_VERSION_PROBE,
        )
        .await;
        let expected = if has_key {
            format!(r#"{{"config":true,"load":"{version}"}}"#)
        } else {
            r#"{"config":false,"load":null}"#.to_string()
        };
        assert_eq!(out, expected, "under vite {version}");
    }
}
