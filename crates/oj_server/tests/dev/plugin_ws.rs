// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A plugin's `server.ws.on("connection")` listener fires for a new client
//! and its `socket.send` reaches that client, and a client custom event
//! reaches `server.ws.on(event)` whose `server.ws.send` reply comes back, on
//! `/__ws` and on a root-dialed vite-hmr socket, over real sockets so hmr.rs's
//! accept and client message routing run.

use serde_json::json;

use crate::listen::{close, custom_event, echoed_protocol, next_frame, send_custom};
use crate::App;

const PLUGINS: &str = r#"export default [{
    name: "ws-echo",
    configureServer(server) {
      server.ws.on("connection", (socket) => {
        socket.send(JSON.stringify({ type: "custom", event: "server:hello", data: { greeted: true } }));
      });
      server.ws.on("client:ping", (data, client) => {
        server.ws.send("server:pong", { echo: data });
      });
    },
  }];
"#;

#[tokio::test(flavor = "multi_thread")]
async fn plugin_ws_connection_and_custom_event_round_trip() {
    let app = App::new(&[
        ("package.json", r#"{"name":"ws-app","version":"1.0.0"}"#),
        ("oj.plugins.mjs", PLUGINS),
        ("src.js", "window.__READY = true;\n"),
        (
            "index.html",
            r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src.js"></script></body></html>"#,
        ),
    ]);
    let server = app.serve().await;
    let listening = server.listen().await;

    let (mut ws, _) = server.ws(listening.addr, "/__ws", None).await;
    assert_eq!(
        custom_event(&mut ws, "server:hello").await,
        json!({ "greeted": true }),
        "server.ws.on('connection') fires for a new client"
    );

    send_custom(&mut ws, "client:ping", json!({ "hello": "world" })).await;
    assert_eq!(
        custom_event(&mut ws, "server:pong").await,
        json!({ "echo": { "hello": "world" } }),
        "plugin ws.on -> ws.send round-trips to the client"
    );
    close(ws).await;

    // The same relay must work for a client on the vite-hmr socket at the root.
    let (mut vws, res) = server.ws(listening.addr, "/", Some("vite-hmr")).await;
    assert_eq!(
        echoed_protocol(&res),
        Some("vite-hmr"),
        "vite-hmr socket opens on /"
    );
    assert_eq!(next_frame(&mut vws).await["type"], "connected");
    send_custom(&mut vws, "client:ping", json!({ "hello": "vite" })).await;
    assert_eq!(
        custom_event(&mut vws, "server:pong").await,
        json!({ "echo": { "hello": "vite" } }),
        "plugin ws relay round-trips over the vite-hmr socket"
    );
    close(vws).await;
}
