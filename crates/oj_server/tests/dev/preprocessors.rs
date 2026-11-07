// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Less and Stylus files imported from JS compile with the app's own `less`
//! and `stylus` packages in dev, and their rules apply in the page (computed
//! colors from each file's variables). The build half is oj's
//! `preprocessors_build`.

use std::time::Duration;

use crate::browser::Page;
use crate::{App, INDEX_HTML};

const WAIT: Duration = Duration::from_secs(20);

const MAIN: &str = "import \"./a.less\";\nimport \"./b.styl\";\n\
const a = document.createElement(\"div\"); a.className = \"box\"; a.id = \"a\"; document.body.appendChild(a);\n\
const b = document.createElement(\"div\"); b.className = \"boxs\"; b.id = \"b\"; document.body.appendChild(b);\n\
window.__READY = true;\n";

const COLORS: &str = r#"[getComputedStyle(document.getElementById("a")).color, getComputedStyle(document.getElementById("b")).color]"#;

#[tokio::test(flavor = "multi_thread")]
async fn less_and_stylus_imports_apply_in_dev() {
    if crate::browser::skip() {
        return;
    }
    crate::require_fixture!("less", "stylus");
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"preproc-app","version":"1.0.0"}"#,
        ),
        ("src/a.less", "@c: rgb(10, 20, 30);\n.box { color: @c; }\n"),
        ("src/b.styl", "d = rgb(40, 50, 60)\n.boxs\n  color d\n"),
        ("src/main.js", MAIN),
        ("index.html", INDEX_HTML),
    ]);
    app.link_fixture("less");
    app.link_fixture("stylus");
    let server = app.serve().await;
    let listening = server.listen().await;
    let page = Page::open(&format!("http://127.0.0.1:{}/", listening.addr.port())).await;
    page.wait_for("window.__READY === true", WAIT).await;
    let [a, b]: [String; 2] = page.eval(COLORS).await;
    assert_eq!(a, "rgb(10, 20, 30)", "less color");
    assert_eq!(b, "rgb(40, 50, 60)", "stylus color");
    assert_eq!(page.errors(), Vec::<String>::new(), "page errors");
    page.close().await;
}
