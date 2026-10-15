// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::fs;

use super::{build_app, files_under, scratch, write_files};

/// UnoCSS-style plugins find the `vite:css-post` plugin in `config.plugins`
/// (captured in configResolved) and, in renderChunk, call its transform with
/// their generated CSS so it routes into the output stylesheet. The virtual
/// `.css` layer module stays a stub; oj's vite:css-post shim must be present
/// and must carry the handed CSS into a linked stylesheet like Vite does.
#[tokio::test]
async fn css_handed_to_the_vite_css_post_shim_reaches_a_linked_stylesheet() {
    let root = scratch("vite-css-post-shim");
    write_files(
        &root,
        &[
            (
                "package.json",
                r#"{"name":"csspost-app","version":"1.0.0"}"#,
            ),
            (
                "src/entry.js",
                "import \"virtual:layer.css\";\ndocument.body.className = \"gen\";\n",
            ),
            (
                "index.html",
                r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/entry.js"></script></body></html>"#,
            ),
            (
                "oj.plugins.mjs",
                r##"let allPlugins;
export default [{
  name: "fake-uno",
  configResolved(config) { allPlugins = config.plugins; },
  resolveId(id) { return id === "virtual:layer.css" ? "\0virtual:layer.css" : null; },
  load(id) { return id === "\0virtual:layer.css" ? "/* layer placeholder */" : null; },
  renderChunk(code, chunk) {
    const hasLayer = Object.keys(chunk.modules || {}).some((m) => m.includes("layer.css"));
    if (!hasLayer) return null;
    const cssPost = (allPlugins || []).find((p) => p && p.name === "vite:css-post");
    if (!cssPost) throw new Error("vite:css-post shim not found in config.plugins");
    const handler = typeof cssPost.transform === "function" ? cssPost.transform : cssPost.transform.handler;
    handler.call(this, ".gen{color:green}", "\0virtual:layer.css");
    return null;
  },
}];
"##,
            ),
        ],
    );
    build_app(&root)
        .await
        .expect("the app builds (the vite:css-post shim is in config.plugins)");

    let css = files_under(&root.join("dist/assets"))
        .into_iter()
        .filter(|(rel, _)| rel.ends_with(".css"))
        .map(|(_, code)| code)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        css.contains(".gen"),
        "CSS handed to the vite:css-post shim reached the output stylesheet:\n{css}"
    );
    let html = fs::read_to_string(root.join("dist/index.html")).expect("dist/index.html");
    assert!(
        html.contains(r#"rel="stylesheet""#),
        "the page links the generated stylesheet:\n{html}"
    );
}
