// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::fs;

use regex::Regex;

use super::{build_app, scratch, write_files};

/// The production html gets a `<link rel="modulepreload" crossorigin>` for
/// each chunk an entry statically imports (Vite's build-html plugin), here a
/// `manualChunks` vendor chunk: the link names that chunk, every preloaded
/// href exists on disk, and the link carries `crossorigin` as Vite emits it.
/// (Whether links appear at all under `build.modulePreload` settings is
/// pinned by `module_preload_configuration_controls_production_links`.)
#[tokio::test]
async fn static_import_chunks_are_preloaded_with_crossorigin() {
    let root = scratch("modulepreload-vendor");
    write_files(
        &root,
        &[
            (
                "package.json",
                r#"{"name":"preload","private":true,"dependencies":{"mylib":"1.0.0"}}"#,
            ),
            (
                "node_modules/mylib/package.json",
                r#"{"name":"mylib","version":"1.0.0","type":"module","main":"index.js"}"#,
            ),
            (
                "node_modules/mylib/index.js",
                "export const hello = 'hi from mylib';\n",
            ),
            (
                "index.html",
                r#"<!doctype html><html><head></head><body><script type="module" src="/src/main.tsx"></script></body></html>"#,
            ),
            (
                "src/main.tsx",
                "import { hello } from \"mylib\";\nwindow.__h = hello;\n",
            ),
            (
                "oj.config.mjs",
                "export default { build: { rollupOptions: { output: { manualChunks: { vendor: [\"mylib\"] } } } } };\n",
            ),
        ],
    );
    build_app(&root)
        .await
        .expect("the modulepreload app builds");

    let dist = root.join("dist");
    let html = fs::read_to_string(dist.join("index.html")).unwrap();
    let links: Vec<String> = Regex::new(r#"<link rel="modulepreload" href="([^"]+)""#)
        .unwrap()
        .captures_iter(&html)
        .map(|c| c[1].to_string())
        .collect();
    assert!(!links.is_empty(), "modulepreload links injected:\n{html}");
    for href in &links {
        assert!(
            dist.join(href.trim_start_matches('/')).exists(),
            "preloaded chunk {href} exists on disk"
        );
    }
    assert!(
        links.iter().any(|h| h.contains("vendor-")),
        "the vendor chunk is preloaded: {links:?}"
    );
    assert!(
        Regex::new(r#"<link rel="modulepreload" href="[^"]+" crossorigin"#)
            .unwrap()
            .is_match(&html),
        "modulepreload links carry crossorigin (Vite sets it):\n{html}"
    );
}
