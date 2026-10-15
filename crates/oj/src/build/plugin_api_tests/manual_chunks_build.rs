// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `build.rolldownOptions.output.manualChunks` in object form puts the named
//! deps in their own `vendor-*.js` chunk, out of the main chunk, and the
//! split chunks link at runtime under preview.

use std::fs;
use std::time::Duration;

use super::browser::Page;
use super::{build_app, files_under, preview, scratch, write_files};

const WAIT: Duration = Duration::from_secs(20);

fn pkg_json(name: &str) -> String {
    format!(r#"{{"name":"{name}","version":"1.0.0","module":"index.js","main":"index.js"}}"#)
}

#[tokio::test(flavor = "multi_thread")]
async fn manual_chunks_object_splits_deps_into_a_vendor_chunk() {
    let root = scratch("manual-chunks-vendor");
    let (a_pkg, b_pkg) = (pkg_json("dep-a"), pkg_json("dep-b"));
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"mc-app","version":"1.0.0"}"#),
            ("node_modules/dep-a/package.json", a_pkg.as_str()),
            (
                "node_modules/dep-a/index.js",
                "export const a = \"DEP_A_MARKER_\" + 42;",
            ),
            ("node_modules/dep-b/package.json", b_pkg.as_str()),
            (
                "node_modules/dep-b/index.js",
                "export const b = \"DEP_B_MARKER_\" + 7;",
            ),
            (
                "src/main.js",
                "import { a } from \"dep-a\";\nimport { b } from \"dep-b\";\nwindow.__V = a + b;\n",
            ),
            (
                "index.html",
                r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>"#,
            ),
            (
                "oj.config.json",
                r#"{"build":{"rolldownOptions":{"output":{"manualChunks":{"vendor":["dep-a","dep-b"]}}}}}"#,
            ),
        ],
    );
    build_app(&root).await.expect("the app builds");

    let dist = root.join("dist");
    let assets = dist.join("assets");
    let files: Vec<String> = fs::read_dir(&assets)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    let vendor = files
        .iter()
        .find(|f| f.starts_with("vendor-") && f.ends_with(".js"))
        .unwrap_or_else(|| panic!("no vendor chunk emitted: {files:?}"));
    let main = files
        .iter()
        .find(|f| f.starts_with("main-") && f.ends_with(".js"))
        .unwrap_or_else(|| panic!("no main chunk emitted: {files:?}"));
    let vendor_code = fs::read_to_string(assets.join(vendor)).unwrap();
    assert!(
        vendor_code.contains("DEP_A_MARKER"),
        "vendor chunk missing dep-a:\n{vendor_code}"
    );
    assert!(
        vendor_code.contains("DEP_B_MARKER"),
        "vendor chunk missing dep-b:\n{vendor_code}"
    );
    let main_code = fs::read_to_string(assets.join(main)).unwrap();
    assert!(
        !main_code.contains("DEP_A_MARKER") && !main_code.contains("DEP_B_MARKER"),
        "deps must be split out of the main chunk:\n{main_code}"
    );

    if super::browser::skip() {
        return;
    }
    let server = preview(&dist, "/").await;
    let page = Page::open(&server.url).await;
    page.wait_for("window.__V", WAIT).await;
    let v: String = page.eval("window.__V").await;
    assert_eq!(
        v,
        "DEP_A_MARKER_42DEP_B_MARKER_7",
        "split chunks did not link at runtime; files: {:?}",
        files_under(&dist)
            .into_iter()
            .map(|(p, _)| p)
            .collect::<Vec<_>>()
    );
    assert_eq!(page.errors(), Vec::<String>::new(), "page errors");
    page.close().await;
}
