// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Vite reserves no `/__` prefix: a navigation under it (a client-routed
//! component preview, `/__component/preview/Badge`) gets the SPA fallback
//! like any route, while oj's own `/__` endpoints stay the dev server's.

use axum::body::Body;
use axum::http::Request;

use crate::App;

#[tokio::test(flavor = "multi_thread")]
async fn a_navigation_under_double_underscore_gets_the_spa_fallback() {
    let app = App::new(&[
        ("package.json", r#"{"name":"dunder-app","private":true}"#),
        ("src/main.js", "export {};\n"),
        (
            "index.html",
            "<!doctype html><html><head><title>spa-shell-marker</title></head><body>\
             <script type=\"module\" src=\"/src/main.js\"></script></body></html>",
        ),
    ]);
    let server = app.serve().await;
    let page = server
        .request(
            Request::get("/__component/preview/Badge")
                .header("accept", "text/html")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(page.status, 200, "{}", page.body);
    assert!(page.body.contains("spa-shell-marker"), "{}", page.body);

    let gate = server.get("/__hmr_gate").await;
    assert!(
        !gate.body.contains("spa-shell-marker"),
        "oj's own endpoint is not a navigation: {}",
        gate.body
    );
}
