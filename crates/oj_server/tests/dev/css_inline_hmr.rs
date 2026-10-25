// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A stylesheet imported `?inline` is a module of its own that never
//! self-accepts, as Vite's css-analysis marks it: its importer holds the
//! text, so an edit reaches the importer (here the entry, so the page
//! reloads, as under Vite). A sheet imported plainly still swaps its
//! `<style>` in place.

use crate::listen::frame_matching;
use crate::{App, INDEX_HTML};

#[tokio::test(flavor = "multi_thread")]
async fn an_inline_sheets_edit_reaches_its_importer() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"css-inline-hmr","version":"1.0.0"}"#,
        ),
        ("src/a.css", ".a { color: red; }\n"),
        ("src/b.css", ".b { color: red; }\n"),
        (
            "src/main.js",
            "import \"./a.css\";\nimport b from \"./b.css?inline\";\nwindow.__B = b;\n",
        ),
        ("index.html", INDEX_HTML),
    ]);
    let server = app.serve().await;
    let listening = server.listen().await;
    let (mut ws, _) = server.ws(listening.addr, "/__ws", None).await;
    // What the page loads: the entry, its sheet and the inline text.
    server.text("/src/main.js").await;
    server.text("/src/a.css").await;
    let inline = server.text("/src/b.css?inline").await;
    assert!(inline.contains("red"), "{inline}");

    app.write(&[("src/b.css", ".b { color: green; }\n")]);
    frame_matching(&mut ws, "a reload for the inline sheet's importer", |f| {
        f["type"] == "full-reload"
    })
    .await;
    let inline = server.text("/src/b.css?inline").await;
    assert!(
        inline.contains("green"),
        "the inline text is fresh: {inline}"
    );

    app.write(&[("src/a.css", ".a { color: green; }\n")]);
    let update = frame_matching(&mut ws, "a hot update for the plain sheet", |f| {
        f["type"] == "update"
    })
    .await;
    let paths: Vec<&str> = update["updates"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|u| u["path"].as_str())
        .collect();
    assert!(
        paths.iter().all(|p| p.starts_with("/src/a.css")),
        "only the plain sheet updates: {update}"
    );
}
