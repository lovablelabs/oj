// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::fs;

use super::{build_app, js_under, scratch, write_files};

/// generateBundle must see a Rollup-shaped bundle: chunks carry
/// facadeModuleId, imports, exports and a `modules` map; a plugin can rename
/// a chunk (mutate fileName) and delete a chunk (the way @crxjs renames pages
/// and removes its manifest JS chunk). Rollup writes whatever the bundle
/// object holds after generateBundle, so a renamed chunk lands at its new
/// fileName and a deleted one is never written.
#[tokio::test]
async fn generate_bundle_sees_rollup_chunks_and_can_rename_or_delete_them() {
    let root = scratch("gbf-fidelity");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"gb-app","version":"1.0.0"}"#),
            ("src/dep.js", "export const dep = 42;\n"),
            (
                "src/entry.js",
                "import { dep } from \"./dep.js\";\nexport const v = dep;\nconsole.log(v);\n",
            ),
            ("src/keep.js", "export const keep = \"keep-chunk-body\";\n"),
            ("src/gone.js", "export const gone = \"gone-chunk-body\";\n"),
            (
                "index.html",
                r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/entry.js"></script></body></html>"#,
            ),
            (
                "oj.plugins.mjs",
                r#"import path from "node:path";
import { fileURLToPath } from "node:url";
const root = path.dirname(fileURLToPath(import.meta.url));
export default [{
  name: "gb-fidelity",
  buildStart(options) {
    if (typeof options.input !== "undefined") {
      this.emitFile({ type: "chunk", id: path.join(root, "src", "keep.js"), name: "keepme" });
      this.emitFile({ type: "chunk", id: path.join(root, "src", "gone.js"), name: "goneme" });
    }
  },
  generateBundle(options, bundle) {
    let info = null;
    let keepExports = null;
    for (const key of Object.keys(bundle)) {
      const c = bundle[key];
      if (c.type === "chunk" && c.isEntry && c.facadeModuleId && c.facadeModuleId.endsWith("index.html")) {
        info = {
          facadeModuleId: c.facadeModuleId,
          imports: c.imports,
          exports: c.exports,
          moduleCount: Object.keys(c.modules).length,
          hasEntry: Object.keys(c.modules).some((m) => m.endsWith("entry.js")),
          moduleIds: c.moduleIds,
          lengths: Object.entries(c.modules).map(([id, m]) => [id.split("/").pop(), m.renderedLength]),
        };
      }
      if (c.type === "chunk" && c.name === "keepme") {
        keepExports = c.exports;
        c.fileName = "assets/renamed-keepme.js";
      }
      if (c.type === "chunk" && c.name === "goneme") delete bundle[key];
    }
    this.emitFile({ type: "asset", fileName: "gb-info.json", source: JSON.stringify({ ...info, keepExports }) });
  },
}];
"#,
            ),
        ],
    );
    build_app(&root).await.expect("the app builds");
    let dist = root.join("dist");

    // Read side: the entry chunk exposed a real Rollup shape.
    let raw =
        fs::read_to_string(dist.join("gb-info.json")).expect("generateBundle wrote gb-info.json");
    let info: serde_json::Value = serde_json::from_str(&raw).expect("gb-info.json is json");
    assert!(
        info.is_object(),
        "the entry chunk was found in generateBundle: {raw}"
    );
    // The page is the entry, as in Vite: its proxy imports the script.
    assert!(
        info["facadeModuleId"]
            .as_str()
            .is_some_and(|id| id.ends_with("index.html")),
        "facadeModuleId is the page's id: {raw}"
    );
    assert!(
        info["imports"].is_array(),
        "chunk.imports is an array: {raw}"
    );
    assert!(
        info["exports"].as_array().is_some_and(|e| e.is_empty()),
        "a page entry exports nothing: {raw}"
    );
    let exports = info["keepExports"]
        .as_array()
        .unwrap_or_else(|| panic!("chunk.exports is an array: {raw}"));
    assert!(
        exports.iter().any(|e| e == "keep"),
        "chunk.exports lists the emitted chunk's export: {raw}"
    );
    assert!(
        info["moduleCount"].as_u64().is_some_and(|n| n >= 1),
        "chunk.modules is a populated map keyed by module id: {raw}"
    );
    assert_eq!(
        info["hasEntry"], true,
        "chunk.modules includes the entry module id: {raw}"
    );
    assert!(
        info["moduleIds"]
            .as_array()
            .is_some_and(|ids| !ids.is_empty()),
        "chunk.moduleIds is populated: {raw}"
    );
    let lengths = info["lengths"]
        .as_array()
        .unwrap_or_else(|| panic!("chunk.modules entries were read: {raw}"));
    assert!(
        lengths
            .iter()
            .any(|l| l[0] == "entry.js" && l[1].as_u64().is_some_and(|n| n > 0)),
        "the entry module reports the length of its rendered code: {raw}"
    );

    // Rename side: the keepme chunk was written under its new name.
    let renamed = fs::read_to_string(dist.join("assets/renamed-keepme.js"))
        .expect("renamed chunk exists at its new fileName");
    assert!(
        renamed.contains("keep-chunk-body"),
        "renamed chunk keeps its content:\n{renamed}"
    );

    // Delete side: the goneme chunk is absent from the output entirely.
    let all_js = js_under(&dist);
    assert!(
        !all_js.contains("gone-chunk-body"),
        "deleted chunk was not written:\n{all_js}"
    );
}
