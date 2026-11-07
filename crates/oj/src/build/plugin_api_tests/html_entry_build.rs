// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! An index.html whose module script has a relative `src` with no leading
//! slash (`src="src/index.tsx"`) builds, as Vite accepts it: the built html
//! points at the hashed `/assets/index-*.js` chunk, and that chunk runs under
//! preview.

use std::fs;
use std::time::Duration;

use regex::Regex;

use super::browser::Page;
use super::{build_app, preview, scratch, write_files};

const WAIT: Duration = Duration::from_secs(20);

#[tokio::test(flavor = "multi_thread")]
async fn relative_script_src_without_leading_slash_builds_and_runs() {
    let root = scratch("html-entry-relative");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"he-app","version":"1.0.0"}"#),
            ("src/index.tsx", "window.__OK = \"relative-entry-works\";\n"),
            (
                "index.html",
                r#"<!doctype html><html lang="en"><head><title>t</title></head><body><div id="app"></div><script type="module" src="src/index.tsx"></script></body></html>"#,
            ),
        ],
    );
    build_app(&root).await.expect("the app builds");

    let dist = root.join("dist");
    let html = fs::read_to_string(dist.join("index.html")).unwrap();
    assert!(
        Regex::new(r#"src="/assets/index-.*\.js""#)
            .unwrap()
            .is_match(&html),
        "built html did not rewrite relative entry to hashed asset:\n{html}"
    );

    if super::browser::skip() {
        return;
    }
    let server = preview(&dist, "/").await;
    let page = Page::open(&server.url).await;
    page.wait_for("window.__OK", WAIT).await;
    let ok: String = page.eval("window.__OK").await;
    assert_eq!(ok, "relative-entry-works", "entry did not run");
    assert_eq!(page.errors(), Vec::<String>::new(), "page errors");
    page.close().await;
}
