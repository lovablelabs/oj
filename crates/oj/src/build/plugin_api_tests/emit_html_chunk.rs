// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::fs;

use super::{build_app, files_under, scratch, write_files};

/// A plugin emits an HTML page as a chunk (the way @crxjs emits its
/// manifest's popup/options pages). oj must treat the `.html` id as a page,
/// as Vite's build-html plugin does for html inputs: extract its
/// `<script type=module>`, bundle those as entries, emit the processed HTML
/// at its root-relative path, and resolve getFileName(refId) to the page's
/// output path. The root index.html still builds alongside it.
#[tokio::test]
async fn an_emitted_html_chunk_is_built_as_a_page_at_its_root_relative_path() {
    let root = scratch("ehc-page");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"hc-app","version":"1.0.0"}"#),
            ("src/entry.js", "console.log(\"entry\");\n"),
            (
                "src/pages/popup/popup.js",
                "export const popup = \"popup-page-body\";\n",
            ),
            (
                "src/pages/popup/index.html",
                r#"<!doctype html><html><head><title>popup</title></head><body><script type="module" src="./popup.js"></script></body></html>"#,
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
  name: "html-page-emitter",
  buildStart(options) {
    if (typeof options.input !== "undefined") {
      ref = this.emitFile({ type: "chunk", id: path.join(root, "src", "pages", "popup", "index.html"), name: "popup" });
    }
  },
  generateBundle() {
    this.emitFile({ type: "asset", fileName: "popup-path.txt", source: this.getFileName(ref) });
  },
}];
"#,
            ),
        ],
    );
    build_app(&root).await.expect("the app builds");
    let dist = root.join("dist");

    // The emitted page was rendered at its root-relative output path.
    let page = fs::read_to_string(dist.join("src/pages/popup/index.html")).unwrap_or_else(|_| {
        panic!(
            "emitted .html page rendered at its root-relative path; dist has: {:?}",
            files_under(&dist)
                .into_iter()
                .map(|(rel, _)| rel)
                .collect::<Vec<_>>()
        )
    });

    // Its page-relative script was bundled and rewritten to a hashed chunk
    // (the e2e's /src="(\/assets\/[^"]+\.js)"/).
    let src = page
        .split("src=\"/assets/")
        .skip(1)
        .find_map(|rest| {
            let end = rest.find('"')?;
            let file = &rest[..end];
            file.ends_with(".js").then(|| format!("assets/{file}"))
        })
        .unwrap_or_else(|| panic!("the page's script was rewritten to a hashed chunk:\n{page}"));
    let chunk = fs::read_to_string(dist.join(&src))
        .unwrap_or_else(|_| panic!("the page's entry chunk {src} was bundled"));
    assert!(
        chunk.contains("popup-page-body"),
        "chunk holds the page's source:\n{chunk}"
    );

    // getFileName(refId) for the emitted page returns its output path.
    let resolved = fs::read_to_string(dist.join("popup-path.txt"))
        .expect("generateBundle wrote popup-path.txt");
    assert_eq!(
        resolved.trim(),
        "src/pages/popup/index.html",
        "getFileName resolves the emitted page path"
    );

    // The root index.html still built.
    assert!(dist.join("index.html").exists(), "root page still built");
}
