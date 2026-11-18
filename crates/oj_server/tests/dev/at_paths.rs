// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Vite reserves only its own `/@` urls (`/@fs/`, `/@id/`, `/@vite/client`,
//! `/@vite/env`): a navigation to any other `/@` path (a route param,
//! `/@someone`) gets the SPA fallback like any route, while oj's own `/@`
//! urls stay the dev server's.

use axum::body::Body;
use axum::http::Request;

use crate::App;

#[tokio::test(flavor = "multi_thread")]
async fn a_navigation_under_at_gets_the_spa_fallback() {
    let app = App::new(&[
        ("package.json", r#"{"name":"at-app","private":true}"#),
        ("src/main.js", "export {};\n"),
        (
            "index.html",
            "<!doctype html><html><head><title>spa-shell-marker</title></head><body>\
             <script type=\"module\" src=\"/src/main.js\"></script></body></html>",
        ),
    ]);
    let server = app.serve().await;
    for path in [
        "/@someone",
        "/@someone/posts",
        "/@someone?tab=1",
        "/%40someone",
        "/@fsomeone",
    ] {
        let page = server
            .request(
                Request::get(path)
                    .header("accept", "text/html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(page.status, 200, "{path}: {}", page.body);
        assert!(
            page.body.contains("spa-shell-marker"),
            "{path}: {}",
            page.body
        );
    }

    let refresh = server.get("/@react-refresh").await;
    assert_eq!(refresh.status, 200, "{}", refresh.body);
    assert!(
        !refresh.body.contains("spa-shell-marker"),
        "oj's own url is not a navigation: {}",
        refresh.body
    );
}
