use super::*;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[tokio::test]
async fn loopback_proxy_returns_redirects_and_cookies_without_contacting_destination() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let destination = format!("http://127.0.0.1:{port}/provider");
    let visits = Arc::new(AtomicUsize::new(0));
    let app = axum::Router::new()
        .route(
            "/redirect/{status}",
            axum::routing::any({
                let destination = destination.clone();
                move |axum::extract::Path(status): axum::extract::Path<u16>| {
                    let destination = destination.clone();
                    async move {
                        Response::builder()
                            .status(status)
                            .header(header::LOCATION, destination)
                            .header(header::SET_COOKIE, "logout_state=pending; HttpOnly; Path=/")
                            .header(header::SET_COOKIE, "session=; Max-Age=0; Path=/")
                            .body(Body::empty())
                            .unwrap()
                    }
                }
            }),
        )
        .route(
            "/provider",
            axum::routing::any({
                let visits = Arc::clone(&visits);
                move || {
                    visits.fetch_add(1, Ordering::SeqCst);
                    async { "The browser must make this request" }
                }
            }),
        );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    for method in ["GET", "POST"] {
        for status in [301, 302, 303, 307, 308] {
            let response = proxy_to_loopback_streaming(
                port,
                method,
                &format!("/redirect/{status}"),
                &HeaderMap::new(),
                (method == "POST").then(|| Body::from("handoff=sealed")),
            )
            .await
            .unwrap();

            assert_eq!(visits.load(Ordering::SeqCst), 0, "{method} {status}");
            assert_eq!(response.status().as_u16(), status, "{method} {status}");
            assert_eq!(response.headers()[header::LOCATION], destination);
            let cookies: Vec<_> = response
                .headers()
                .get_all(header::SET_COOKIE)
                .iter()
                .map(|value| value.to_str().unwrap())
                .collect();
            assert_eq!(
                cookies,
                [
                    "logout_state=pending; HttpOnly; Path=/",
                    "session=; Max-Age=0; Path=/"
                ]
            );
        }
    }

    server.abort();
}

// Every client that forwards the browser's request (`state.http` for the
// Rust `server.proxy` and its insecure-TLS twin, the loopback hop) leaves a
// redirect to the browser, as Vite's http-proxy (`followRedirects: false`)
// and its in-process plugin middleware do.
#[tokio::test]
async fn browser_hop_clients_never_follow_a_redirect() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let visits = Arc::new(AtomicUsize::new(0));
    let app = axum::Router::new()
        .route(
            "/out",
            axum::routing::any(|| async {
                Response::builder()
                    .status(302)
                    .header(header::LOCATION, "/next")
                    .header(header::SET_COOKIE, "session=; Max-Age=0; Path=/")
                    .body(Body::empty())
                    .unwrap()
            }),
        )
        .route(
            "/next",
            axum::routing::any({
                let visits = Arc::clone(&visits);
                move || {
                    visits.fetch_add(1, Ordering::SeqCst);
                    async { "followed" }
                }
            }),
        );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    for insecure in [false, true] {
        let resp = browser_hop_client(insecure)
            .post(format!("http://127.0.0.1:{port}/out"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 302, "insecure: {insecure}");
        assert_eq!(resp.headers()[header::LOCATION], "/next");
        assert_eq!(
            resp.headers()[header::SET_COOKIE],
            "session=; Max-Age=0; Path=/"
        );
    }
    assert_eq!(visits.load(Ordering::SeqCst), 0);
    server.abort();
}

/// A fall-through names the url a middleware left in `req.url` only when it
/// is an origin-form path other than the one asked for: an absolute or
/// protocol-relative url (a proxy `bypass` may return one) never reaches
/// native routing, and an unchanged url is no rewrite.
#[test]
fn a_fall_through_reports_an_origin_form_rewrite_only() {
    let with = |url: &str| {
        let mut headers = HeaderMap::new();
        headers.insert("x-oj-fallthrough", "1".parse().unwrap());
        headers.insert("x-oj-rewritten-url", url.parse().unwrap());
        headers
    };
    let asked = "/services/fire";
    let rewritten = |url: &str| rewritten_url(&with(url), asked).map(|u| u.to_string());
    assert_eq!(
        rewritten("/services/fire/index.html"),
        Some("/services/fire/index.html".to_string())
    );
    assert_eq!(rewritten("/about?tab=1"), Some("/about?tab=1".to_string()));
    assert_eq!(rewritten(asked), None, "the url as asked is no rewrite");
    assert_eq!(rewritten("//evil.example/x"), None);
    assert_eq!(rewritten("https://evil.example/x"), None);
    assert_eq!(rewritten("about"), None);
    assert_eq!(rewritten_url(&HeaderMap::new(), asked), None);
}
