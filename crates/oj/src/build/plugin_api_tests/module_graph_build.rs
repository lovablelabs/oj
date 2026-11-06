// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::fs;

use super::{build_app, js_under, scratch, write_files};

const INDEX_HTML: &str = r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/entry.ts"></script></body></html>"#;

/// Under `vite build`, `this.getModuleInfo` and `this.getModuleIds` are
/// rolldown's, so they know every module of the bundle, including the ones a
/// hook filter kept from the plugin. A filtered resolveId asking for its
/// importer's info gets it though the importer's transform never crossed.
#[tokio::test]
async fn a_filtered_resolve_id_reads_its_importers_info() {
    let root = scratch("graph-resolve");
    write_files(
        &root,
        &[
            (
                "package.json",
                r#"{"name":"graph-resolve","version":"1.0.0"}"#,
            ),
            ("index.html", INDEX_HTML),
            (
                "src/entry.ts",
                "import { other } from \"./other.ts\";\nconsole.log(other);\n",
            ),
            (
                "src/other.ts",
                "import { probe } from \"virtual:probe\";\nexport const other = probe;\n",
            ),
            (
                "oj.plugins.mjs",
                r#"export default [
  {
    name: "graph-probe",
    resolveId: {
      filter: { id: /^virtual:probe$/ },
      handler(source, importer) {
        const info = this.getModuleInfo(importer);
        globalThis.__probe = info ? `importer-known:${info.id.split("/").pop()}` : "importer-unknown";
        return "\0virtual:probe";
      },
    },
    load: {
      filter: { id: /^\0virtual:probe$/ },
      handler() {
        return `export const probe = ${JSON.stringify(globalThis.__probe)};`;
      },
    },
    transform: {
      filter: { id: /\.css$/ },
      handler() {},
    },
  },
];
"#,
            ),
        ],
    );
    build_app(&root).await.expect("the probe app builds");
    let assets = js_under(&root.join("dist/assets"));
    assert!(
        assets.contains("importer-known:other.ts"),
        "resolveId read its importer's module info:\n{assets}"
    );
    let [_, _, transform] = gate_counts(&root);
    assert!(
        transform > 0,
        "the gate kept the scripts from the transform"
    );
}

/// A plugin with bundle-stage hooks no longer pins transform unfiltered: the
/// getters it may call there read rolldown's graph. They see the modules the
/// gate kept away, with their importers, exports and code (as JS), and the
/// `meta` a transform set stays on the module.
#[tokio::test]
async fn bundle_hooks_read_the_whole_graph_with_the_transform_gated() {
    let root = scratch("graph-bundle");
    write_files(
        &root,
        &[
            (
                "package.json",
                r#"{"name":"graph-bundle","version":"1.0.0"}"#,
            ),
            ("index.html", INDEX_HTML),
            (
                "src/entry.ts",
                "import { other } from \"./other.ts\";\nconsole.log(other);\n",
            ),
            (
                "src/other.ts",
                "const n: number = 2;\nexport const other = n;\n",
            ),
            (
                "oj.plugins.mjs",
                r#"export default [
  {
    name: "graph-reader",
    transform: {
      filter: { id: /entry\.ts$/ },
      handler(code) {
        return { code, map: null, meta: { graphReader: "entry-meta" } };
      },
    },
    generateBundle() {
      const ids = [...this.getModuleIds()];
      const find = (end) => this.getModuleInfo(ids.find((id) => id.endsWith(end)));
      const other = find("/other.ts");
      const entry = find("/entry.ts");
      const short = (list) => list.map((id) => id.split("/").pop());
      this.emitFile({
        type: "asset",
        fileName: "graph.json",
        source: JSON.stringify({
          otherImporters: other && short(other.importers),
          otherExports: other && other.exports,
          otherCode: other && other.code,
          entryImports: entry && short(entry.importedIds),
          entryMeta: entry && entry.meta,
        }),
      });
    },
  },
];
"#,
            ),
        ],
    );
    build_app(&root).await.expect("the graph app builds");
    let graph: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(root.join("dist/graph.json")).expect("generateBundle wrote graph.json"),
    )
    .unwrap();
    assert_eq!(
        graph["otherImporters"],
        serde_json::json!(["entry.ts"]),
        "{graph}"
    );
    assert_eq!(
        graph["otherExports"],
        serde_json::json!(["other"]),
        "{graph}"
    );
    let code = graph["otherCode"].as_str().unwrap_or_default();
    assert!(
        code.contains("other") && !code.contains(": number"),
        "the code reads as JS: {graph}"
    );
    assert_eq!(
        graph["entryImports"],
        serde_json::json!(["other.ts"]),
        "{graph}"
    );
    assert_eq!(graph["entryMeta"]["graphReader"], "entry-meta", "{graph}");
    let [_, _, transform] = gate_counts(&root);
    assert!(
        transform > 0,
        "generateBundle no longer pins the transform unfiltered"
    );
}

/// The one build of `root`'s gated-out RPC counts `[resolveId, load, transform]`.
fn gate_counts(root: &std::path::Path) -> [u64; 3] {
    match crate::build::gate_skips(root).as_slice() {
        [counts] => *counts,
        other => panic!(
            "expected one closeBundle gate report for {}, got {other:?}",
            root.display()
        ),
    }
}
