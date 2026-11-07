// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The build half of the Less and Stylus check: `oj build` compiles both with
//! the app's own `less` and `stylus` packages, and in the preview their rules
//! apply (computed colors from each file's variables) with no page errors.

use std::time::Duration;

use super::browser::Page;
use super::{build_app, link_fixtures, preview, scratch, write_files};

const WAIT: Duration = Duration::from_secs(20);

const MAIN: &str = "import \"./a.less\";\nimport \"./b.styl\";\n\
const a = document.createElement(\"div\"); a.className = \"box\"; a.id = \"a\"; document.body.appendChild(a);\n\
const b = document.createElement(\"div\"); b.className = \"boxs\"; b.id = \"b\"; document.body.appendChild(b);\n\
window.__READY = true;\n";

const COLORS: &str = r#"[getComputedStyle(document.getElementById("a")).color, getComputedStyle(document.getElementById("b")).color]"#;

#[tokio::test(flavor = "multi_thread")]
async fn less_and_stylus_imports_apply_in_preview() {
    if super::browser::skip() {
        return;
    }
    let root = scratch("preprocessors");
    if !link_fixtures(&root, &["less", "stylus"]) {
        return;
    }
    write_files(
        &root,
        &[
            (
                "package.json",
                r#"{"name":"preproc-app","version":"1.0.0"}"#,
            ),
            ("src/a.less", "@c: rgb(10, 20, 30);\n.box { color: @c; }\n"),
            ("src/b.styl", "d = rgb(40, 50, 60)\n.boxs\n  color d\n"),
            ("src/main.js", MAIN),
            (
                "index.html",
                r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>"#,
            ),
        ],
    );
    build_app(&root).await.expect("the less/stylus app builds");

    let preview = preview(&root.join("dist"), "/").await;
    let page = Page::open(&preview.url).await;
    page.wait_for("window.__READY === true", WAIT).await;
    let [a, b]: [String; 2] = page.eval(COLORS).await;
    assert_eq!(a, "rgb(10, 20, 30)", "less color");
    assert_eq!(b, "rgb(40, 50, 60)", "stylus color");
    assert_eq!(page.errors(), Vec::<String>::new(), "page errors");
    page.close().await;
}
