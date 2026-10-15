// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Vite's asset query suffixes in dev, as the page sees them: `?url` gives
//! the served path, `?raw` the file's text, `?inline` a data: URL, and
//! `?init` an instantiate function for a wasm module.

use std::time::Duration;

use crate::browser::Page;
use crate::App;

const WAIT: Duration = Duration::from_secs(20);

/// A wasm module exporting `add(i32, i32) -> i32`.
const ADD_WASM: &[u8] = &[
    0, 0x61, 0x73, 0x6d, 1, 0, 0, 0, 1, 7, 1, 0x60, 2, 0x7f, 0x7f, 1, 0x7f, 3, 2, 1, 0, 7, 7, 1, 3,
    0x61, 0x64, 0x64, 0, 0, 10, 9, 1, 7, 0, 0x20, 0, 0x20, 1, 0x6a, 0x0b,
];

#[tokio::test(flavor = "multi_thread")]
async fn url_raw_inline_and_init_queries_in_dev() {
    if crate::browser::skip() {
        return;
    }
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"qassets-app","version":"1.0.0"}"#,
        ),
        (
            "src/pic.svg",
            r#"<svg xmlns="http://www.w3.org/2000/svg"></svg>"#,
        ),
        ("src/note.txt", "hello-raw-content"),
        ("src/data.json", r#"{"k":42}"#),
        (
            "src/main.js",
            "import picUrl from \"./pic.svg?url\";\n\
             import raw from \"./note.txt?raw\";\n\
             import inlineData from \"./data.json?inline\";\n\
             import init from \"./add.wasm?init\";\n\
             window.__PICURL = picUrl;\n\
             window.__RAW = raw;\n\
             window.__INLINE = inlineData.startsWith(\"data:\");\n\
             init().then((i) => { window.__SUM = i.exports.add(4, 5); });\n",
        ),
        ("index.html", crate::INDEX_HTML),
    ]);
    std::fs::write(app.root.join("src/add.wasm"), ADD_WASM).unwrap();
    let server = app.serve().await;
    let listening = server.listen().await;
    let page = Page::open(&format!("http://127.0.0.1:{}/", listening.addr.port())).await;
    page.wait_for("window.__SUM !== undefined", WAIT).await;
    let r: serde_json::Value = page
        .eval(
            "({ url: window.__PICURL, raw: window.__RAW, inline: window.__INLINE, sum: window.__SUM })",
        )
        .await;
    assert_eq!(r["url"], "/src/pic.svg", "?url: {r}");
    assert_eq!(r["raw"], "hello-raw-content", "?raw: {r}");
    assert_eq!(r["inline"], true, "?inline: {r}");
    assert_eq!(r["sum"], 9, "?init add: {r}");
    assert_eq!(page.errors(), Vec::<String>::new(), "page errors");
    page.close().await;
}
