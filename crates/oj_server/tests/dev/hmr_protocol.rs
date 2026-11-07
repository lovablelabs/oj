// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The served HMR client picks its socket scheme from `location.protocol`
//! (wss behind an https proxy, never a hardcoded `ws://` + host), and over
//! plain http the socket still connects: an edit to a non-accepting entry
//! lands as a reload in the page.

use std::time::Duration;

use regex::Regex;

use crate::browser::Page;
use crate::App;

const WAIT: Duration = Duration::from_secs(20);

#[tokio::test(flavor = "multi_thread")]
async fn client_derives_ws_scheme_and_reloads_over_http() {
    if crate::browser::skip() {
        return;
    }
    let app = App::new(&[
        ("package.json", r#"{"name":"hmr-app","version":"1.0.0"}"#),
        (
            "src/main.js",
            "document.title = \"v1\"; window.__READY = true;\n",
        ),
        ("index.html", crate::INDEX_HTML),
    ]);
    let server = app.serve().await;

    let client = server.text("/@oj/client.js").await;
    assert!(
        client.contains(r#"location.protocol === "https:" ? "wss" : "ws""#),
        "client.js derives wss:\n{client}"
    );
    assert!(
        !Regex::new(r#""ws://"\s*\+\s*location\.host"#)
            .unwrap()
            .is_match(&client),
        "no hardcoded ws:// left:\n{client}"
    );

    let listening = server.listen().await;
    let page = Page::open(&format!("http://127.0.0.1:{}/", listening.addr.port())).await;
    page.wait_for("window.__READY === true", WAIT).await;
    app.write(&[(
        "src/main.js",
        "document.title = \"v2\"; window.__READY = true;\n",
    )]);
    page.wait_for(r#"document.title === "v2""#, WAIT).await;
    assert_eq!(page.errors(), Vec::<String>::new(), "page errors");
    page.close().await;
}
