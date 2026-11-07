// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::fs;

use super::{build_app, scratch, write_files};

/// The Vite build manifest must be present in the generateBundle bundle as a
/// `.vite/manifest.json` asset (Vite's manifest plugin emits it from its own
/// generateBundle, so later plugins see it), so plugins that read it there,
/// e.g. @crxjs's web-accessible-resources, can. Here a plugin reads it and
/// re-emits it. Note the plugin finds it without `build.manifest` set.
#[tokio::test]
async fn generate_bundle_sees_the_vite_manifest_asset() {
    let root = scratch("vbm-in-bundle");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"vm-app","version":"1.0.0"}"#),
            ("src/entry.js", "export const v = 1;\nconsole.log(v);\n"),
            (
                "index.html",
                r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/entry.js"></script></body></html>"#,
            ),
            (
                "oj.plugins.mjs",
                r#"export default [{
  name: "reads-vite-manifest",
  generateBundle(options, bundle) {
    const m = bundle[".vite/manifest.json"];
    if (m && m.type === "asset" && typeof m.source === "string") {
      this.emitFile({ type: "asset", fileName: "seen-manifest.json", source: m.source });
    }
  },
}];
"#,
            ),
        ],
    );
    build_app(&root).await.expect("the app builds");

    let raw = fs::read_to_string(root.join("dist/seen-manifest.json"))
        .expect("the plugin read .vite/manifest.json from the generateBundle bundle");
    let manifest: serde_json::Value =
        serde_json::from_str(&raw).expect("the re-emitted manifest is json");
    let entry = manifest
        .as_object()
        .and_then(|m| m.values().find(|e| e["isEntry"] == true))
        .unwrap_or_else(|| panic!("the manifest lists an entry:\n{raw}"));
    assert!(
        entry["file"].as_str().is_some_and(|f| f.ends_with(".js")),
        "the entry maps to a js chunk:\n{raw}"
    );
}
