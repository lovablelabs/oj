// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Asset urls in dev, as Vite serves them: a bare `import url from "./pic.svg"`
//! is the file's root-relative path (`/src/pic.svg`), `new URL("./pic.svg",
//! import.meta.url)` resolves to that same file, and the url the page got
//! fetches with 200.

use std::time::Duration;

use crate::browser::Page;
use crate::{App, INDEX_HTML};

const WAIT: Duration = Duration::from_secs(20);

#[tokio::test(flavor = "multi_thread")]
async fn svg_import_and_new_url_resolve_to_the_served_file() {
    if crate::browser::skip() {
        return;
    }
    let app = App::new(&[
        ("package.json", r#"{"name":"asset-app","version":"1.0.0"}"#),
        (
            "src/pic.svg",
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect width="10" height="10"/></svg>"#,
        ),
        (
            "src/main.js",
            "import picUrl from \"./pic.svg\";\nconst asUrl = new URL(\"./pic.svg\", import.meta.url);\nwindow.__PIC = picUrl;\nwindow.__ASURL = asUrl.href;\n",
        ),
        ("index.html", INDEX_HTML),
    ]);
    let server = app.serve().await;
    let listening = server.listen().await;
    let page = Page::open(&format!("http://127.0.0.1:{}/", listening.addr.port())).await;
    page.wait_for(
        "window.__PIC !== undefined && window.__ASURL !== undefined",
        WAIT,
    )
    .await;

    let pic: String = page.eval("window.__PIC").await;
    let as_url: String = page.eval("window.__ASURL").await;
    // Fetched from the page, so it is the url the browser resolves.
    let status: u16 = page.eval("fetch(window.__PIC).then((r) => r.status)").await;
    assert_eq!(pic, "/src/pic.svg", "bare import url");
    assert!(as_url.ends_with("/src/pic.svg"), "new URL: {as_url}");
    assert_eq!(status, 200, "asset fetch of {pic}");
    assert_eq!(page.errors(), Vec::<String>::new(), "page errors");
    page.close().await;
}
