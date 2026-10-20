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

/// An upstream that echoes what reached it: the x-forwarded-* headers on
/// `/xf/*`, and on `/api/replay` the method and body behind a 307.
async fn echo_backend() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = axum::Router::new()
        .route(
            "/api/replay",
            axum::routing::any(|| async {
                axum::response::Response::builder()
                    .status(307)
                    .header(header::LOCATION, "/api/landed")
                    .body(Body::empty())
                    .unwrap()
            }),
        )
        .route(
            "/api/landed",
            axum::routing::any(|method: axum::http::Method, body: String| async move {
                format!("{method} {body}")
            }),
        )
        .fallback(|headers: axum::http::HeaderMap| async move {
            let get = |n: &str| {
                headers
                    .get(n)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("-")
                    .to_string()
            };
            format!(
                "for={} port={} proto={} host={} via={}",
                get("x-forwarded-for"),
                get("x-forwarded-port"),
                get("x-forwarded-proto"),
                get("x-forwarded-host"),
                get("x-via")
            )
        });
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    port
}

/// A plugin middleware that reports the socket it sees, as Vite's would.
const WHOAMI: &str = r#"{
    name: "whoami",
    configureServer(server) {
      server.middlewares.use((req, res, next) => {
        if (req.url !== "/whoami") return next();
        res.end(`${req.socket.remoteAddress} ${!!req.socket.encrypted}`);
      });
    },
  }"#;

/// http-proxy's `xfwd` and Vite's `req.socket`: the plugin host sees the
/// browser's connection, not the loopback hop's, and a client cannot claim
/// another one.
#[tokio::test(flavor = "multi_thread")]
async fn plugin_host_sees_the_browser_connection() {
    let port = echo_backend().await;
    let config = format!(
        r#"export default {{
  plugins: [{WHOAMI}],
  server: {{ proxy: {{
    "/xf": {{ target: "http://127.0.0.1:{port}", xfwd: true }},
    "/api": {{ target: "http://127.0.0.1:{port}", followRedirects: true }},
  }} }},
}};
"#
    );
    let app = App::new(&[
        ("package.json", r#"{"name":"proxy-xfwd","version":"1.0.0"}"#),
        ("vite.config.mjs", &config),
        ("index.html", crate::INDEX_HTML),
    ]);
    let server = app.serve().await;
    middleware_ready(&server).await;
    let browser = oj_server::tls::ClientConn {
        addr: "203.0.113.7:5555".parse().unwrap(),
        encrypted: true,
    };
    let request = |uri: &str| {
        Request::builder()
            .uri(uri.to_string())
            .header(header::HOST, "localhost:8443")
            .header("x-forwarded-for", "10.0.0.1")
            .body(Body::empty())
            .unwrap()
    };

    let res = oj_server::tls::with_client_conn(browser, server.request(request("/xf/a"))).await;
    assert_eq!(
        res.body,
        "for=10.0.0.1,203.0.113.7 port=8443 proto=https host=localhost:8443 via=-"
    );
    let res = oj_server::tls::with_client_conn(browser, server.request(request("/whoami"))).await;
    assert_eq!(res.body, "203.0.113.7 true");

    // A client naming its own address is not believed.
    let mut spoof = request("/whoami");
    spoof
        .headers_mut()
        .insert("x-oj-remote-address", "6.6.6.6".parse().unwrap());
    spoof
        .headers_mut()
        .insert("x-oj-encrypted", "1".parse().unwrap());
    let res = server.request(spoof).await;
    assert!(!res.body.contains("6.6.6.6"), "{}", res.body);
    assert!(res.body.ends_with(" false"), "{}", res.body);

    // followRedirects replays a POST and its body across a 307, as
    // follow-redirects does.
    let res = server
        .request(
            Request::builder()
                .method("POST")
                .uri("/api/replay")
                .body(Body::from("payload"))
                .unwrap(),
        )
        .await;
    assert_eq!((res.status, res.body.as_str()), (200, "POST payload"));
}

/// Over a real connection: `serve_router` records the peer, so a plugin and
/// `xfwd` see the browser's address. Served on IPv6 loopback so the browser
/// (`::1`) is told apart from the loopback hop to the plugin host
/// (`127.0.0.1`).
#[tokio::test(flavor = "multi_thread")]
async fn a_real_connection_reaches_the_plugin_host() {
    let Ok(listener) = tokio::net::TcpListener::bind("[::1]:0").await else {
        eprintln!("skipped: no IPv6 loopback");
        return;
    };
    let port = echo_backend().await;
    let config = format!(
        r#"export default {{
  plugins: [{WHOAMI}],
  server: {{ proxy: {{ "/xf": {{ target: "http://127.0.0.1:{port}", xfwd: true }} }} }},
}};
"#
    );
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"proxy-xfwd-tcp","version":"1.0.0"}"#,
        ),
        ("vite.config.mjs", &config),
        ("index.html", crate::INDEX_HTML),
    ]);
    let server = app.serve().await;
    middleware_ready(&server).await;
    let addr = listener.local_addr().unwrap();
    let router = server.built.router.clone();
    let task = tokio::spawn(async move {
        let _ = oj_server::tls::serve_router(listener, router, None).await;
    });
    let base = format!("http://[::1]:{}", addr.port());
    let get = |path: &str| {
        let url = format!("{base}{path}");
        async move { reqwest::get(url).await.unwrap().text().await.unwrap() }
    };
    assert_eq!(get("/whoami").await, "::1 false");
    // `port=1` is http-proxy's getPort on an IPv6 Host: the first
    // `:<digits>` of `[::1]:<port>` is the address's own `:1`.
    assert_eq!(
        get("/xf/a").await,
        format!("for=::1 port=1 proto=http host=[::1]:{} via=-", addr.port())
    );
    task.abort();
}

/// A self-signed certificate for localhost, 127.0.0.1 and ::1 (test only).
const TEST_CERT: &str = "-----BEGIN CERTIFICATE-----
MIIBrjCCAVSgAwIBAgIUYyJPYjyjo9/yCbSx0PcTA382SNMwCgYIKoZIzj0EAwIw
FDESMBAGA1UEAwwJbG9jYWxob3N0MCAXDTI2MTAyMDIxMDMyOVoYDzIxMjYwOTI2
MjEwMzI5WjAUMRIwEAYDVQQDDAlsb2NhbGhvc3QwWTATBgcqhkjOPQIBBggqhkjO
PQMBBwNCAATxt7qeWXT7ENpOS6PfsUSa4CKA3pXEeL9HdsivgY/WuplPn3K+Z6Ve
Ap5soKQzBmnwSpSv03o/ahGFkjnrNzOfo4GBMH8wHQYDVR0OBBYEFHOPK8N/zgR1
6TtY12Jh4PKuEFqhMB8GA1UdIwQYMBaAFHOPK8N/zgR16TtY12Jh4PKuEFqhMA8G
A1UdEwEB/wQFMAMBAf8wLAYDVR0RBCUwI4IJbG9jYWxob3N0hwR/AAABhxAAAAAA
AAAAAAAAAAAAAAABMAoGCCqGSM49BAMCA0gAMEUCIQC2+dJk9w/7diFtuLjAZ4Dh
5pYCsH7CUXeo8dr0T7wZ2QIgBqKPwIfmlNDjzgxyP79Wc8fwVUt236JjLzRzsn/c
0CE=
-----END CERTIFICATE-----";
const TEST_KEY: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgc13hX/6lh+OiDDdc
hH3KBqWHl/9QKd5uEKuebmpI1sKhRANCAATxt7qeWXT7ENpOS6PfsUSa4CKA3pXE
eL9HdsivgY/WuplPn3K+Z6VeAp5soKQzBmnwSpSv03o/ahGFkjnrNzOf
-----END PRIVATE KEY-----";

/// The echo backend's `/xf` route behind `xfwd`, plus the WHOAMI plugin, and
/// with `via_lib` a `configure` hook only http-proxy runs (the built-in pipe
/// cannot), which stamps `x-via` on what reaches the backend.
fn xfwd_app(port: u16, via_lib: bool) -> App {
    let configure = if via_lib {
        r#", configure(proxy) { proxy.on("proxyReq", (req) => req.setHeader("x-via", "http-proxy-3")); }"#
    } else {
        ""
    };
    let config = format!(
        r#"export default {{
  plugins: [{WHOAMI}],
  server: {{ proxy: {{ "/xf": {{ target: "http://127.0.0.1:{port}", xfwd: true{configure} }} }} }},
}};
"#
    );
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"proxy-xfwd-tls","version":"1.0.0"}"#,
        ),
        ("vite.config.mjs", &config),
        ("index.html", crate::INDEX_HTML),
    ]);
    if via_lib {
        app.link_fixture("http-proxy-3");
    }
    app
}

/// Serves the server's router over TLS through `serve_router`, as `oj dev`
/// with `server.https` does, and returns its base URL.
async fn serve_tls(server: &crate::Server) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = server.built.router.clone();
    let https = oj_config::HttpsConfig {
        key: TEST_KEY.to_string(),
        cert: TEST_CERT.to_string(),
    };
    let task = tokio::spawn(async move {
        let _ = oj_server::tls::serve_router(listener, router, Some(&https)).await;
    });
    (format!("https://127.0.0.1:{}", addr.port()), task)
}

async fn get_tls(base: &str, path: &str) -> String {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap()
        .get(format!("{base}{path}"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
}

/// Over HTTPS the plugin host sees a TLS socket, and `xfwd` says https, on
/// the built-in pipe.
#[tokio::test(flavor = "multi_thread")]
async fn tls_connection_reaches_the_plugin_host() {
    let port = echo_backend().await;
    let app = xfwd_app(port, false);
    let server = app.serve().await;
    middleware_ready(&server).await;
    let (base, task) = serve_tls(&server).await;
    let tls_port = base.rsplit(':').next().unwrap().to_string();
    assert_eq!(get_tls(&base, "/whoami").await, "127.0.0.1 true");
    assert_eq!(
        get_tls(&base, "/xf/a").await,
        format!("for=127.0.0.1 port={tls_port} proto=https host=127.0.0.1:{tls_port} via=-")
    );
    task.abort();
}

/// The same through the app's own http-proxy-3, which tells TLS from
/// `instanceof TLSSocket` or the legacy `pair` mark.
#[tokio::test(flavor = "multi_thread")]
async fn tls_connection_reaches_the_apps_http_proxy() {
    crate::require_fixture!("http-proxy-3");
    let port = echo_backend().await;
    let app = xfwd_app(port, true);
    let server = app.serve().await;
    middleware_ready(&server).await;
    let (base, task) = serve_tls(&server).await;
    let tls_port = base.rsplit(':').next().unwrap().to_string();
    assert_eq!(
        get_tls(&base, "/xf/a").await,
        format!(
            "for=127.0.0.1 port={tls_port} proto=https host=127.0.0.1:{tls_port} via=http-proxy-3"
        )
    );
    task.abort();
}
