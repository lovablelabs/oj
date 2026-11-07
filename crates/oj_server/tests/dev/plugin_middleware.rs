// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! configureServer middleware gets the request body of a POST the router
//! does not claim and its response comes back unchanged, and
//! transformIndexHtml rewrites the served index. The plugin also pokes the
//! watcher, moduleGraph and ws the way Vite bridges do, which must not break
//! either path.

use std::time::Duration;

use axum::body::Body;
use axum::http::Request;

use crate::{App, Server};

const PLUGINS: &str = r#"export default [{
    name: "bridge-like",
    configureServer(server) {
      const orig = server.watcher.emit.bind(server.watcher);
      server.watcher.emit = (e, ...a) => orig(e, ...a);
      server.moduleGraph.onFileChange("/x");
      server.ws.on("connection", () => {});
      server.middlewares.use((req, res, next) => {
        if (req.method === "POST" && req.url === "/__plugin/echo") {
          let body = "";
          req.on("data", (c) => (body += c));
          req.on("end", () => {
            res.setHeader("content-type", "application/json");
            res.statusCode = 200;
            res.end(JSON.stringify({ echoed: JSON.parse(body || "{}") }));
          });
          return;
        }
        next();
      });
    },
    transformIndexHtml(html) {
      return html.replace("</head>", '<script>window.__INJECTED = true;</script></head>');
    },
  }];
"#;

/// Harness addition: the middleware port can arrive after boot (a late
/// serve-info push), and until then unmatched requests are not forwarded.
pub async fn middleware_ready(server: &Server) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while server.built.plugin_serve.mw_port().is_none() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "plugin middleware port never came up"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn post_body_reaches_plugin_middleware_and_index_html_is_transformed() {
    let app = App::new(&[
        ("package.json", r#"{"name":"mw-app","version":"1.0.0"}"#),
        ("oj.plugins.mjs", PLUGINS),
        ("src.js", "window.__READY = true;\n"),
        (
            "index.html",
            r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src.js"></script></body></html>"#,
        ),
    ]);
    let server = app.serve().await;
    middleware_ready(&server).await;

    let echo = server
        .request(
            Request::post("/__plugin/echo")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"hi":1,"nested":{"a":"b"}}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(echo.status, 200, "{}", echo.body);
    let echoed: serde_json::Value = serde_json::from_str(&echo.body)
        .unwrap_or_else(|e| panic!("echo reply is JSON ({e}):\n{}", echo.body));
    assert_eq!(
        echoed,
        serde_json::json!({ "echoed": { "hi": 1, "nested": { "a": "b" } } }),
        "POST body forwarded to plugin middleware"
    );

    let html = server.text("/").await;
    assert!(
        html.contains("window.__INJECTED = true"),
        "transformIndexHtml injection present:\n{html}"
    );
}
