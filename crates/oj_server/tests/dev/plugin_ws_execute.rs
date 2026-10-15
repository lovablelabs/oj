// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A plugin middleware that broadcasts a command over `server.ws.send`, waits
//! for a client's reply event and answers the HTTP request with it: the
//! broadcast, the client's reply frame into `server.ws.on`, and the held
//! response all round-trip. The client is a real `/__ws` socket playing the
//! browser.

use axum::body::Body;
use axum::http::Request;
use serde_json::json;

use crate::listen::{close, custom_event, send_custom};
use crate::plugin_middleware::middleware_ready;
use crate::App;

const PLUGINS: &str = r#"let seq = 0;
   const pending = new Map();
   export default [{
     name: "exec-bridge",
     configureServer(server) {
       server.ws.on("exec:result", (data) => {
         const p = pending.get(data.requestId);
         if (p) { pending.delete(data.requestId); p(data.value); }
       });
       server.middlewares.use((req, res, next) => {
         if (req.method !== "POST" || req.url !== "/exec") return next();
         const requestId = String(++seq);
         const done = new Promise((resolve) => {
           pending.set(requestId, resolve);
           setTimeout(() => { pending.delete(requestId); resolve(null); }, 4000);
         });
         server.ws.send("exec:run", { requestId, code: "40 + 2" });
         done.then((value) => {
           res.setHeader("content-type", "application/json");
           res.statusCode = 200;
           res.end(JSON.stringify({ requestId, value }));
         });
       });
     },
   }];
"#;

#[tokio::test(flavor = "multi_thread")]
async fn plugin_broadcast_client_reply_and_http_response_round_trip() {
    let app = App::new(&[
        ("package.json", r#"{"name":"exec-app","version":"1.0.0"}"#),
        ("oj.plugins.mjs", PLUGINS),
        ("src.js", "window.__READY = true;\n"),
        (
            "index.html",
            r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src.js"></script></body></html>"#,
        ),
    ]);
    let server = app.serve().await;
    middleware_ready(&server).await;
    let listening = server.listen().await;
    let (mut ws, _) = server.ws(listening.addr, "/__ws", None).await;

    let client = async {
        let run = custom_event(&mut ws, "exec:run").await;
        // A real client would eval the code; here it is a fixed expression.
        assert_eq!(run["code"], "40 + 2", "exec:run carries the code: {run}");
        let reply = json!({ "requestId": run["requestId"], "value": 40 + 2 });
        send_custom(&mut ws, "exec:result", reply).await;
        run["requestId"].clone()
    };
    let (res, request_id) = tokio::join!(
        server.request(Request::post("/exec").body(Body::empty()).unwrap()),
        client
    );
    assert_eq!(res.status, 200, "{}", res.body);
    let out: serde_json::Value = serde_json::from_str(&res.body)
        .unwrap_or_else(|e| panic!("exec reply is JSON ({e}):\n{}", res.body));
    assert_eq!(
        out["value"], 42,
        "plugin broadcast -> client eval -> plugin collect -> http response round-trips: {out}"
    );
    assert_eq!(
        out["requestId"], request_id,
        "the reply answers the broadcast request"
    );
    close(ws).await;
}
