// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The build half of the template-literal dynamic import check: `oj build`
//! code-splits each page the import can reach into its own chunk (named after
//! the page), and the preview loads both the boot-time and the later page in
//! a real browser with no page errors.

use std::time::Duration;

use super::browser::Page;
use super::{build_app, preview, scratch, write_files};

const WAIT: Duration = Duration::from_secs(20);

const MAIN: &str = "async function load(which) { const m = await import(`./pages/${which}.js`); return m.name; }\n\
window.__LOAD = load;\n\
load(\"home\").then((n) => { window.__HOME = n; });\n";

#[tokio::test(flavor = "multi_thread")]
async fn template_dynamic_import_code_splits_pages_and_loads_them_in_preview() {
    let root = scratch("dynamic-import");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"dynimp-app","version":"1.0.0"}"#),
            ("src/pages/home.js", "export const name = \"home-page\";\n"),
            (
                "src/pages/about.js",
                "export const name = \"about-page\";\n",
            ),
            ("src/main.js", MAIN),
            (
                "index.html",
                r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>"#,
            ),
        ],
    );
    build_app(&root)
        .await
        .expect("the dynamic-import app builds");

    let assets: Vec<String> = std::fs::read_dir(root.join("dist/assets"))
        .expect("dist/assets exists")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        assets.iter().any(|f| f.starts_with("home-"))
            && assets.iter().any(|f| f.starts_with("about-")),
        "prod build did not code-split pages: {assets:?}"
    );

    if super::browser::skip() {
        return;
    }
    let preview = preview(&root.join("dist"), "/").await;
    let page = Page::open(&preview.url).await;
    page.wait_for("window.__HOME !== undefined", WAIT).await;
    let home: String = page.eval("window.__HOME").await;
    let about: String = page.eval(r#"window.__LOAD("about")"#).await;
    assert_eq!(home, "home-page", "boot-time dynamic import");
    assert_eq!(
        about, "about-page",
        "dynamic import called later from the page"
    );
    assert_eq!(page.errors(), Vec::<String>::new(), "page errors");
    page.close().await;
}
