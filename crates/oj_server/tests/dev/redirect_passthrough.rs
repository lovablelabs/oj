// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A redirect answered behind oj reaches the browser as written, as in Vite:
//! plugin middleware runs inside Vite's own connect app (nothing in between
//! to follow anything), and `server.proxy` keeps http-proxy's
//! `followRedirects: false`, with no `Location` or cookie rewrite unless the
//! config asks for one. Following a redirect on the server serves the
//! destination under the requested URL and drops the `Set-Cookie` headers a
//! sign-out sets, so the browser never completes the flow.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request};

use crate::plugin_middleware::middleware_ready;
use crate::App;

const COOKIES: [&str; 2] = [
    "session=; Max-Age=0; Path=/",
    "logout=done; HttpOnly; Path=/",
];

fn set_cookies(res: &crate::Response) -> Vec<&str> {
    res.headers
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap())
        .collect()
}

/// An upstream whose `/api/out/<status>` redirects to `/api/next` with two
/// cookies; `/api/next` counts the requests that reach it.
async fn redirecting_backend(visits: Arc<AtomicUsize>) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = axum::Router::new()
        .route(
            "/api/out/{status}",
            axum::routing::any(
                move |axum::extract::Path(status): axum::extract::Path<u16>| async move {
                    let mut res = axum::response::Response::builder()
                        .status(status)
                        .header(header::LOCATION, "/api/next");
                    for cookie in COOKIES {
                        res = res.header(header::SET_COOKIE, cookie);
                    }
                    res.body(Body::empty()).unwrap()
                },
            ),
        )
        .route(
            "/api/next",
            axum::routing::any(move || {
                visits.fetch_add(1, Ordering::SeqCst);
                async { "DESTINATION" }
            }),
        );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    port
}

#[tokio::test(flavor = "multi_thread")]
async fn server_proxy_passes_a_redirect_and_its_cookies_to_the_browser() {
    let visits = Arc::new(AtomicUsize::new(0));
    let port = redirecting_backend(Arc::clone(&visits)).await;
    let config = format!(
        "export default {{ server: {{ proxy: {{ \"/api\": \"http://127.0.0.1:{port}\" }} }} }};\n"
    );
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"proxy-redirect","version":"1.0.0"}"#,
        ),
        ("vite.config.mjs", &config),
        ("index.html", crate::INDEX_HTML),
    ]);
    let server = app.serve().await;
    // The path every vite.config app takes: oj -> the loopback hop -> the
    // plugin host's http-proxy -> the backend. (The Rust-only fallback shares
    // the same no-redirect client; plugin_mw's unit tests pin it.)
    middleware_ready(&server).await;

    for method in ["GET", "POST"] {
        for status in [302u16, 303, 307] {
            let res = server
                .request(
                    Request::builder()
                        .method(method)
                        .uri(format!("/api/out/{status}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await;
            assert_eq!(res.status, status, "{method} {status}: {}", res.body);
            assert_eq!(
                res.headers[header::LOCATION],
                "/api/next",
                "{method} {status}"
            );
            assert_eq!(set_cookies(&res), COOKIES, "{method} {status}");
        }
    }
    assert_eq!(
        visits.load(Ordering::SeqCst),
        0,
        "oj followed a redirect the browser must follow"
    );
}

/// A sign-out middleware: clears the session and redirects to an absolute
/// URL built from the request's Host, as auth libraries do.
const PLUGINS: &str = r#"export default [{
    name: "sign-out",
    configureServer(server) {
      server.middlewares.use((req, res, next) => {
        if (req.url === "/auth/sign-out") {
          res.writeHead(302, {
            Location: `http://${req.headers.host}/signed-out`,
            "Set-Cookie": ["session=; Max-Age=0; Path=/", "logout=done; HttpOnly; Path=/"],
          });
          res.end();
          return;
        }
        if (req.url === "/signed-out") {
          res.end("DESTINATION");
          return;
        }
        next();
      });
    },
  }];
"#;

#[tokio::test(flavor = "multi_thread")]
async fn plugin_middleware_redirect_reaches_the_browser_with_its_host() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"mw-redirect","version":"1.0.0"}"#,
        ),
        ("oj.plugins.mjs", PLUGINS),
        ("index.html", crate::INDEX_HTML),
    ]);
    let server = app.serve().await;
    middleware_ready(&server).await;

    for method in ["POST", "GET"] {
        let res = server
            .request(
                Request::builder()
                    .method(method)
                    .uri("/auth/sign-out")
                    .header(header::HOST, "localhost:8443")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(res.status, 302, "{method}: {}", res.body);
        // The middleware saw the browser's Host, not the loopback one.
        assert_eq!(
            res.headers[header::LOCATION],
            "http://localhost:8443/signed-out",
            "{method}"
        );
        assert_eq!(set_cookies(&res), COOKIES, "{method}");
        assert!(!res.body.contains("DESTINATION"), "{method}: {}", res.body);
    }
}

/// An upstream for the opt-in options: `/api/abs` redirects to an absolute
/// URL on itself with a backend-scoped cookie; `/api/next` counts visits.
async fn absolute_redirect_backend(visits: Arc<AtomicUsize>) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = axum::Router::new()
        .route(
            "/api/abs",
            axum::routing::any(move || async move {
                axum::response::Response::builder()
                    .status(302)
                    .header(
                        header::LOCATION,
                        format!("http://127.0.0.1:{port}/api/next"),
                    )
                    .header(header::SET_COOKIE, "sid=1; Domain=backend.test; Path=/api")
                    .body(Body::empty())
                    .unwrap()
            }),
        )
        .route(
            "/api/next",
            axum::routing::any(move |headers: axum::http::HeaderMap| {
                visits.fetch_add(1, Ordering::SeqCst);
                let host = headers
                    .get(header::HOST)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                async move { format!("DESTINATION host={host}") }
            }),
        );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    port
}

/// http-proxy's opt-in options, as Vite applies them: `autoRewrite` moves a
/// target-host `Location` to the browser's Host, `cookieDomainRewrite` and
/// `cookiePathRewrite` rewrite `Set-Cookie`, `followRedirects` follows on
/// the server.
async fn assert_opt_in_options(server: &crate::Server, visits: &AtomicUsize) {
    let get = |uri: &str| {
        Request::builder()
            .uri(uri.to_string())
            .header(header::HOST, "localhost:8443")
            .body(Body::empty())
            .unwrap()
    };
    let res = server.request(get("/api/abs")).await;
    assert_eq!(res.status, 302, "{}", res.body);
    assert_eq!(
        res.headers[header::LOCATION],
        "http://localhost:8443/api/next"
    );
    assert_eq!(set_cookies(&res), ["sid=1; Domain=localhost; Path=/"]);
    assert_eq!(visits.load(Ordering::SeqCst), 0);

    let res = server.request(get("/follow/abs")).await;
    assert_eq!(res.status, 200, "{}", res.body);
    assert!(res.body.starts_with("DESTINATION"), "{}", res.body);
    assert_eq!(visits.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn plugin_host_proxy_applies_http_proxy_opt_in_options() {
    let visits = Arc::new(AtomicUsize::new(0));
    let port = absolute_redirect_backend(Arc::clone(&visits)).await;
    let config = format!(
        r#"export default {{ server: {{ proxy: {{
  "/api": {{ target: "http://127.0.0.1:{port}", autoRewrite: true, cookieDomainRewrite: "localhost", cookiePathRewrite: {{ "/api": "/" }} }},
  "/follow": {{ target: "http://127.0.0.1:{port}", followRedirects: true, rewrite: (p) => p.replace(/^\/follow/, "/api") }},
}} }} }};
"#
    );
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"proxy-opt-in","version":"1.0.0"}"#,
        ),
        ("vite.config.mjs", &config),
        ("index.html", crate::INDEX_HTML),
    ]);
    let server = app.serve().await;
    // The plugin host's node pipe: this app has no resolvable http-proxy.
    middleware_ready(&server).await;
    assert_opt_in_options(&server, &visits).await;
}

/// The same options from an oj config cross to the plugin host's proxy as
/// the config wrote them.
#[tokio::test(flavor = "multi_thread")]
async fn oj_config_proxy_applies_http_proxy_opt_in_options() {
    let visits = Arc::new(AtomicUsize::new(0));
    let port = absolute_redirect_backend(Arc::clone(&visits)).await;
    let config = format!(
        r#"{{ "server": {{ "proxy": {{
  "/api": {{ "target": "http://127.0.0.1:{port}", "autoRewrite": true, "cookieDomainRewrite": "localhost", "cookiePathRewrite": {{ "/api": "/" }} }},
  "/follow": {{ "target": "http://127.0.0.1:{port}", "followRedirects": true, "rewrite": {{ "from": "^/follow", "to": "/api" }} }}
}} }} }}"#
    );
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"proxy-oj-config","version":"1.0.0"}"#,
        ),
        ("oj.config.json", &config),
        ("index.html", crate::INDEX_HTML),
    ]);
    let server = app.serve().await;
    middleware_ready(&server).await;
    assert_opt_in_options(&server, &visits).await;
}

/// Vite's string shorthand is `{ target, changeOrigin: true }`: the target
/// sees its own Host.
#[tokio::test(flavor = "multi_thread")]
async fn string_proxy_entry_changes_origin() {
    let visits = Arc::new(AtomicUsize::new(0));
    let port = absolute_redirect_backend(Arc::clone(&visits)).await;
    let config =
        format!(r#"{{ "server": {{ "proxy": {{ "/api": "http://127.0.0.1:{port}" }} }} }}"#);
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"proxy-shorthand","version":"1.0.0"}"#,
        ),
        ("oj.config.json", &config),
        ("index.html", crate::INDEX_HTML),
    ]);
    let server = app.serve().await;
    middleware_ready(&server).await;
    let res = server
        .request(
            Request::builder()
                .uri("/api/next")
                .header(header::HOST, "localhost:8443")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(res.body, format!("DESTINATION host=127.0.0.1:{port}"));
}
