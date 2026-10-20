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
