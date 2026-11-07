// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::fs;

use super::{build_app, scratch, write_files};

/// The `src="/assets/....js"` value of the first module script in `html`.
fn hashed_script_src(html: &str) -> Option<&str> {
    let start = html.find(r#"src="/assets/"#)? + r#"src=""#.len();
    let len = html[start..].find('"')?;
    let src = &html[start..start + len];
    src.ends_with(".js").then_some(src)
}

/// A project with NO root index.html takes its entries from
/// build.rolldownOptions.input, as Vite does: two HTML pages in nested
/// directories, each with a page-relative <script>. Each page is emitted at
/// its root-relative path with its script rewritten to its own hashed,
/// server-absolute entry chunk, and the manifest keys both entries by source
/// path. The build must not fail with "no index.html".
#[tokio::test]
async fn explicit_html_inputs_build_without_a_root_index_html() {
    let root = scratch("multi-html-input");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"mh-app","version":"1.0.0"}"#),
            (
                "oj.config.json",
                r#"{"build":{"manifest":true,"rolldownOptions":{"input":{"admin":"pages/admin/index.html","app":"pages/app/index.html"}}}}"#,
            ),
            ("pages/admin/admin.js", "document.title = \"admin\";\n"),
            ("pages/app/app.js", "document.title = \"app\";\n"),
            (
                "pages/admin/index.html",
                r#"<!doctype html><html><head><title>admin</title></head><body><script type="module" src="./admin.js"></script></body></html>"#,
            ),
            (
                "pages/app/index.html",
                r#"<!doctype html><html><head><title>app</title></head><body><script type="module" src="./app.js"></script></body></html>"#,
            ),
        ],
    );
    build_app(&root)
        .await
        .expect("explicit html inputs build without a root index.html");

    let dist = root.join("dist");
    let admin_html = fs::read_to_string(dist.join("pages/admin/index.html"))
        .expect("admin page emitted at its root-relative path");
    let app_html = fs::read_to_string(dist.join("pages/app/index.html"))
        .expect("app page emitted at its root-relative path");

    let admin_src = hashed_script_src(&admin_html)
        .unwrap_or_else(|| panic!("admin html references a hashed chunk:\n{admin_html}"));
    let app_src = hashed_script_src(&app_html)
        .unwrap_or_else(|| panic!("app html references a hashed chunk:\n{app_html}"));
    assert_ne!(admin_src, app_src, "each page has its own entry chunk");
    for src in [admin_src, app_src] {
        assert!(
            dist.join(src.trim_start_matches('/')).exists(),
            "chunk {src} exists"
        );
    }

    let manifest = fs::read_to_string(dist.join(".vite/manifest.json")).expect("manifest written");
    let manifest: serde_json::Value = serde_json::from_str(&manifest).expect("manifest is JSON");
    let keys: Vec<&String> = manifest
        .as_object()
        .expect("manifest is an object")
        .keys()
        .collect();
    assert!(
        keys.iter().any(|k| *k == "pages/admin/index.html"),
        "manifest keys the admin entry by its page, as Vite: {keys:?}"
    );
    assert!(
        keys.iter().any(|k| *k == "pages/app/index.html"),
        "manifest keys the app entry by its page, as Vite: {keys:?}"
    );
}
