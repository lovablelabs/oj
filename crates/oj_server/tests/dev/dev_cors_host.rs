// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Vite's dev-server request policies through the real router: `server.cors`
//! (default: localhost origins only, `false` off, an object with exact
//! origins and credentials) and `server.allowedHosts` (Host allowlist against
//! DNS rebinding: localhost, IP literals and listed hosts pass, a dotted
//! entry covers subdomains, everything else is 403), plus the WebSocket
//! Origin check. `localhost_origin_default_matches_vite_regex`,
//! `host_policy_allows_localhost_ips_and_configured_hosts_only` and
//! `cors_policy_forms` pin the policy types; this pins the middleware wiring.

use axum::body::Body;
use axum::http::{Method, Request};

use crate::{App, Response, Server, INDEX_HTML};

const ACAO: &str = "access-control-allow-origin";

async fn serve_with(config: Option<&str>) -> (App, Server) {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"corshost-app","version":"1.0.0"}"#,
        ),
        ("src/main.js", "window.__OK = 1;\n"),
        ("index.html", INDEX_HTML),
    ]);
    if let Some(config) = config {
        app.write(&[("oj.config.json", config)]);
    }
    let server = app.serve().await;
    (app, server)
}

async fn req(server: &Server, method: Method, path: &str, headers: &[(&str, &str)]) -> Response {
    let mut b = Request::builder().method(method).uri(path);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    server.request(b.body(Body::empty()).unwrap()).await
}

async fn get(server: &Server, path: &str, headers: &[(&str, &str)]) -> Response {
    req(server, Method::GET, path, headers).await
}

fn header(res: &Response, name: &str) -> Option<String> {
    let all: Vec<&str> = res
        .headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect();
    (!all.is_empty()).then(|| all.join(", "))
}

/// A websocket upgrade the router can complete in-process: `oneshot` has no
/// connection to upgrade, so the request carries hyper's no-op `OnUpgrade`
/// (what `hyper::upgrade::on` returns without one), which axum's extractor
/// accepts. The handler's Origin check runs before anything awaits it.
async fn ws_upgrade(server: &Server, path: &str, origin: &str) -> Response {
    let mut r = Request::get(path)
        .header("host", "localhost:5491")
        .header("origin", origin)
        .header("connection", "Upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .body(Body::empty())
        .unwrap();
    let on_upgrade = hyper::upgrade::on(&mut r);
    r.extensions_mut().insert(on_upgrade);
    server.request(r).await
}

#[tokio::test(flavor = "multi_thread")]
async fn default_cors_reflects_localhost_and_host_check_blocks_foreign_hosts() {
    let (_app, server) = serve_with(None).await;

    let r = get(
        &server,
        "/src/main.js",
        &[("origin", "http://localhost:1234")],
    )
    .await;
    assert_eq!(
        header(&r, ACAO).as_deref(),
        Some("http://localhost:1234"),
        "localhost origin reflected"
    );
    assert!(
        header(&r, "vary").unwrap_or_default().contains("Origin"),
        "Vary: Origin"
    );
    let r = get(
        &server,
        "/src/main.js",
        &[("origin", "http://app.localhost:1234")],
    )
    .await;
    assert_eq!(
        header(&r, ACAO).as_deref(),
        Some("http://app.localhost:1234"),
        "*.localhost origin reflected"
    );
    let r = get(&server, "/src/main.js", &[("origin", "http://evil.com")]).await;
    assert_eq!(header(&r, ACAO), None, "foreign origin gets no CORS header");

    let r = req(
        &server,
        Method::OPTIONS,
        "/src/main.js",
        &[
            ("origin", "http://localhost:1234"),
            ("access-control-request-method", "GET"),
            ("access-control-request-headers", "x-custom"),
        ],
    )
    .await;
    assert_eq!(r.status, 204, "preflight is answered");
    assert!(
        header(&r, "access-control-allow-methods")
            .unwrap_or_default()
            .contains("GET"),
        "allow-methods: {:?}",
        header(&r, "access-control-allow-methods")
    );
    assert_eq!(
        header(&r, "access-control-allow-headers").as_deref(),
        Some("x-custom")
    );

    assert_eq!(
        get(&server, "/", &[("host", "localhost:5491")])
            .await
            .status,
        200
    );
    assert_eq!(
        get(&server, "/", &[("host", "127.0.0.1:5491")])
            .await
            .status,
        200
    );
    assert_eq!(
        get(&server, "/", &[("host", "evil.com")]).await.status,
        403,
        "foreign Host is blocked (DNS rebinding)"
    );

    assert_eq!(
        ws_upgrade(&server, "/__ws", "http://evil.com").await.status,
        403,
        "ws upgrade from a foreign Origin is refused"
    );
    // The Vite-client socket (vite-hmr subprotocol) is a separate path, a
    // middleware before any route, so it needs the same Origin rule.
    let r = get(
        &server,
        "/",
        &[
            ("origin", "http://evil.com"),
            ("connection", "Upgrade"),
            ("upgrade", "websocket"),
            ("sec-websocket-version", "13"),
            ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
            ("sec-websocket-protocol", "vite-hmr"),
        ],
    )
    .await;
    assert_eq!(
        r.status, 403,
        "vite-hmr upgrade from a foreign Origin is refused"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cors_false_and_allowed_hosts_list() {
    let (_app, server) = serve_with(Some(
        r#"{"server":{"cors":false,"allowedHosts":["evil.com",".corp.example"]}}"#,
    ))
    .await;

    let r = get(
        &server,
        "/src/main.js",
        &[("origin", "http://localhost:1234")],
    )
    .await;
    assert_eq!(header(&r, ACAO), None, "cors:false adds no header");
    assert_eq!(
        get(&server, "/", &[("host", "evil.com")]).await.status,
        200,
        "allowedHosts entry passes"
    );
    assert_eq!(
        get(&server, "/", &[("host", "a.b.corp.example:80")])
            .await
            .status,
        200,
        "dotted entry allows subdomains"
    );
    assert_eq!(
        get(&server, "/", &[("host", "other.example")]).await.status,
        403,
        "unlisted Host is still blocked"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cors_origin_object_and_allowed_hosts_true() {
    let (_app, server) = serve_with(Some(
        r#"{"server":{"cors":{"origin":"http://foo.test","credentials":true},"allowedHosts":true}}"#,
    ))
    .await;

    let r = get(&server, "/src/main.js", &[("origin", "http://foo.test")]).await;
    assert_eq!(header(&r, ACAO).as_deref(), Some("http://foo.test"));
    assert_eq!(
        header(&r, "access-control-allow-credentials").as_deref(),
        Some("true")
    );
    let r = get(
        &server,
        "/src/main.js",
        &[("origin", "http://localhost:1234")],
    )
    .await;
    assert_eq!(
        header(&r, ACAO),
        None,
        "an explicit origin list replaces the localhost default"
    );
    assert_eq!(
        get(&server, "/", &[("host", "anything.example")])
            .await
            .status,
        200,
        "allowedHosts:true disables the check"
    );
}
