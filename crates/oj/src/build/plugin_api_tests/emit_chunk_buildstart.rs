// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::fs;

use super::{build_app, scratch, write_files};

/// A plugin emits its root chunk in buildStart (the way @crxjs emits its
/// manifest chunk) and reads that chunk's hashed name via getFileName() in
/// generateBundle. buildStart runs as a rolldown hook, so the emitted chunk
/// becomes a build root; it emits only when `options.input` is defined,
/// mirroring @crxjs's manifest-loader, so this also pins that buildStart
/// receives the input options.
#[tokio::test]
async fn a_chunk_emitted_in_build_start_becomes_a_named_build_root() {
    let root = scratch("ecbs-buildstart");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"bs-app","version":"1.0.0"}"#),
            ("src/entry.js", "console.log(\"entry\");\n"),
            (
                "src/background.js",
                "export const bg = \"background-chunk-loaded\";\n",
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
  name: "buildstart-emitter",
  buildStart(options) {
    if (typeof options.input !== "undefined") {
      ref = this.emitFile({ type: "chunk", id: path.join(root, "src", "background.js"), name: "background" });
    }
  },
  generateBundle() {
    this.emitFile({ type: "asset", fileName: "bg-name.txt", source: this.getFileName(ref) });
  },
}];
"#,
            ),
        ],
    );
    build_app(&root).await.expect("the app builds");

    let name = fs::read_to_string(root.join("dist/bg-name.txt"))
        .expect("generateBundle wrote the chunk name asset");
    let name = name.trim();
    assert!(
        name.contains("background"),
        "getFileName resolved the buildStart-emitted chunk: {name}"
    );
    assert!(
        name.ends_with(".js"),
        "the resolved name is a js chunk: {name}"
    );
    let chunk = fs::read_to_string(root.join("dist").join(name))
        .unwrap_or_else(|_| panic!("emitted chunk {name} was bundled as a build root"));
    assert!(
        chunk.contains("background-chunk-loaded"),
        "chunk contains its source:\n{chunk}"
    );
}
