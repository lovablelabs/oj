//! `server.proxy` from vite.config is adopted and forwards matching requests
//! to the target with the original path, like Vite's proxy middleware.

use crate::App;

/// An upstream that echoes the request path.
async fn backend() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = axum::Router::new().fallback(|uri: axum::http::Uri| async move {
        let path = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
        format!("BACKEND:{path}")
    });
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    port
}

#[tokio::test(flavor = "multi_thread")]
async fn vite_config_proxy_forwards_to_the_backend() {
    let port = backend().await;
    // 127.0.0.1, not localhost: the backend binds IPv4 only.
    let config = format!(
        "export default {{ server: {{ proxy: {{ \"/api\": \"http://127.0.0.1:{port}\" }} }}, worker: {{ format: \"es\" }} }};\n"
    );
    let app = App::new(&[
        ("package.json", r#"{"name":"proxy-app","version":"1.0.0"}"#),
        ("vite.config.mjs", &config),
        (
            "index.html",
            "<!doctype html><html><head><title>t</title></head><body>hi</body></html>",
        ),
    ]);
    let server = app.serve().await;

    // What `oj dev` prints as "proxy: /api".
    assert_eq!(
        server.built.proxy_prefixes,
        vec!["/api".to_string()],
        "oj did not adopt vite.config server.proxy"
    );
    let body = server.text("/api/hello").await;
    assert_eq!(body, "BACKEND:/api/hello", "proxy did not forward");
}
