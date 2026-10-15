// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The editor HMR gate: a socket gets `lovable:dev-server-mode` (classic)
//! and `lovable:boot-progress` (numeric module counts) on connect; an edit
//! while gated is held (no update, no reload) and listed by
//! `GET /__hmr_gate`; `POST /__hmr_flush` releases it as a full reload.
//! Turned on with `server.hmrGate` in oj.config.json, the same switch the
//! OJ_HMR_GATE env knob flips (env cannot be set per test).

use std::time::Duration;

use axum::body::Body;
use axum::http::Request;

use crate::browser::Page;
use crate::listen::next_text_within;
use crate::App;

const WAIT: Duration = Duration::from_secs(20);

#[tokio::test(flavor = "multi_thread")]
async fn gated_edit_is_held_until_flush_then_reloads() {
    if crate::browser::skip() {
        return;
    }
    let app = App::new(&[
        ("package.json", r#"{"name":"gate-app","version":"1.0.0"}"#),
        ("oj.config.json", r#"{"server":{"hmrGate":true}}"#),
        (
            "src/main.js",
            "document.title = \"v1\"; window.__READY = true;\n",
        ),
        ("index.html", crate::INDEX_HTML),
    ]);
    let server = app.serve().await;
    let listening = server.listen().await;

    let (mut ws, _) = server.ws(listening.addr, "/__ws", None).await;
    let mut mode = None;
    let mut boot = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
    while mode.is_none() || boot.is_none() {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let text = next_text_within(&mut ws, left)
            .await
            .unwrap_or_else(|| panic!("missing on-connect frames (mode={mode:?}, boot={boot:?})"));
        let Ok(frame) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        if frame["type"] != "custom" {
            continue;
        }
        match frame["event"].as_str() {
            Some("lovable:dev-server-mode") => mode = Some(frame["data"].clone()),
            Some("lovable:boot-progress") => boot = Some(frame["data"].clone()),
            _ => {}
        }
    }
    let (mode, boot) = (mode.unwrap(), boot.unwrap());
    assert_eq!(
        mode["mode"], "classic",
        "gate announces classic mode on connect"
    );
    assert!(
        boot["ssrModules"].is_number(),
        "boot-progress ssrModules is a number: {boot}"
    );
    assert!(
        boot["clientModules"].is_number(),
        "boot-progress clientModules is a number: {boot}"
    );
    crate::listen::close(ws).await;

    let page = Page::open(&format!("http://127.0.0.1:{}/", listening.addr.port())).await;
    page.wait_for("window.__READY === true", WAIT).await;
    page.eval::<bool>(r#"(window.__MARK = "kept", true)"#).await;
    assert_eq!(page.eval::<String>("document.title").await, "v1");

    app.write(&[(
        "src/main.js",
        "document.title = \"v2\"; window.__READY = true;\n",
    )]);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        page.eval::<String>("document.title").await,
        "v1",
        "edit is held: title unchanged before flush"
    );
    assert_eq!(
        page.eval::<Option<String>>("window.__MARK ?? null")
            .await
            .as_deref(),
        Some("kept"),
        "no reload happened before flush"
    );

    let status: serde_json::Value =
        serde_json::from_str(&server.text("/__hmr_gate").await).unwrap();
    assert_eq!(status["enabled"], true, "gate enabled: {status}");
    assert!(
        status["count"].as_u64().is_some_and(|c| c >= 1),
        "gate has pending changes: {status}"
    );

    let res = server
        .request(Request::post("/__hmr_flush").body(Body::empty()).unwrap())
        .await;
    assert_eq!(res.status, 200, "flush:\n{}", res.body);
    let flush: serde_json::Value = serde_json::from_str(&res.body).unwrap();
    assert_eq!(
        flush["mode"], "full-reload",
        "flush reports full-reload mode: {flush}"
    );
    assert!(
        flush["count"].as_u64().is_some_and(|c| c >= 1),
        "flush released changes: {flush}"
    );

    page.wait_for(r#"document.title === "v2""#, WAIT).await;
    assert!(
        page.eval::<bool>("window.__MARK === undefined").await,
        "page reloaded on flush (marker gone)"
    );
    assert_eq!(page.errors(), Vec::<String>::new(), "page errors");
    page.close().await;
}
