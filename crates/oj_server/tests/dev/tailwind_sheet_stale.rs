// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A served Tailwind sheet picks up a class added in a source file whether or
//! not an update goes out: with `server.hmr: false`, and after the gate's
//! full-reload flush (which sends no css-update). The default-config control
//! proves the edit itself reaches the sheet.

use std::time::Duration;

use axum::body::Body;
use axum::http::Request;

use crate::{require_fixture, App};

const SHEET: &str = "/src/index.css?direct";

fn component(class: &str) -> String {
    format!("export const A = () => <h1 className=\"{class}\">hi</h1>;\n")
}

async fn sheet_gains_new_class(config: (&str, &str), flush: bool) {
    require_fixture!("tailwindcss", "@tailwindcss");
    let app = App::new(&[
        ("package.json", r#"{"name":"tw-stale","version":"1.0.0"}"#),
        config,
        ("src/index.css", "@import \"tailwindcss\";\n"),
        ("src/App.tsx", &component("underline")),
        ("index.html", crate::INDEX_HTML),
    ]);
    app.link_fixture("tailwindcss");
    app.link_fixture("@tailwindcss");
    let server = app.serve().await;
    let _listening = server.listen().await;

    let first = server.text(SHEET).await;
    assert!(first.contains(".underline"), "seeded sheet: {first}");
    assert!(!first.contains(".italic"), "seeded sheet has no italic");

    app.write(&[("src/App.tsx", &component("underline italic"))]);
    if flush {
        // The edit must be held before the flush can release it.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let res = server
            .request(Request::post("/__hmr_flush").body(Body::empty()).unwrap())
            .await;
        assert_eq!(res.status, 200, "flush:\n{}", res.body);
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !server.text(SHEET).await.contains(".italic") {
        assert!(
            tokio::time::Instant::now() < deadline,
            "sheet stayed stale after the edit"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn hmr_off_sheet_picks_up_an_edit() {
    sheet_gains_new_class(
        (
            "vite.config.mjs",
            "export default { server: { hmr: false } };\n",
        ),
        false,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn gated_flush_sheet_picks_up_an_edit() {
    sheet_gains_new_class(("oj.config.json", r#"{"server":{"hmrGate":true}}"#), true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn default_sheet_picks_up_an_edit() {
    sheet_gains_new_class(("vite.config.mjs", "export default {};\n"), false).await;
}
