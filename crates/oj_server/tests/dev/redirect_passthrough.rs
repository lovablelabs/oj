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
