// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::fs;

use super::{build_app, scratch, write_files};

/// A plugin emits an extra chunk from a real source file during transform,
/// then reads that chunk's final hashed name via getFileName() in
/// generateBundle and writes it into an asset (the crx spine):
/// emitFile({type:'chunk'}) bundles a new entry, getFileName(refId) names it.
#[tokio::test]
async fn an_emitted_chunk_is_bundled_and_named_by_get_file_name() {
    let root = scratch("emit-chunk");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"ec-app","version":"1.0.0"}"#),
            ("src/entry.js", "console.log(\"entry\");\n"),
            (
                "src/content.js",
                "export const marker = \"content-chunk-loaded\";\n",
            ),
            (
                "index.html",
                r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/entry.js"></script></body></html>"#,
            ),
            (
                "oj.plugins.mjs",
                r#"import path from "node:path";
import { fileURLToPath } from "node:url";
const root = path.dirname(fileURLToPath(import.meta.url));
let ref;
export default [{
  name: "chunk-emitter",
  transform(code, id) {
    if (id.endsWith("entry.js")) {
      ref = this.emitFile({ type: "chunk", id: path.join(root, "src", "content.js"), name: "content" });
    }
    return null;
  },
  generateBundle() {
    this.emitFile({ type: "asset", fileName: "emitted-chunk-name.txt", source: this.getFileName(ref) });
  },
}];
"#,
            ),
        ],
    );
    build_app(&root).await.expect("the app builds");

    let name = fs::read_to_string(root.join("dist/emitted-chunk-name.txt"))
        .expect("generateBundle wrote the chunk name asset");
    let name = name.trim();
    assert!(
        name.contains("content") && name.ends_with(".js"),
        "getFileName resolved to the emitted js chunk's name: {name}"
    );
    let chunk = fs::read_to_string(root.join("dist").join(name))
        .unwrap_or_else(|_| panic!("emitted chunk {name} was actually bundled"));
    assert!(
        chunk.contains("content-chunk-loaded"),
        "the emitted chunk contains its source:\n{chunk}"
    );
}
