// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Harness addition: the built router on a real loopback port, for what
//! oneshot cannot do (a websocket upgrade), and a real client for it. The
//! socket accept, the subprotocol echo and the client message parsing in
//! hmr.rs all run as under `oj dev`.

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::client::Response;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::Server;

pub type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Positive waits: generous for a loaded CI box, still bounded.
pub const WAIT: Duration = Duration::from_secs(10);

/// The serving task; dropping it stops accepting.
pub struct Listening {
    pub addr: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Listening {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Server {
    /// Serves the router on 127.0.0.1:<any> the way `DevServer::run` does
    /// (plain http through `tls::serve_router`). Binds directly rather than
    /// through `bind_dev_listener`: that announces the port to every live
    /// plugin host in the process, which would reach other tests' hosts.
    pub async fn listen(&self) -> Listening {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = self.built.router.clone();
        let task = tokio::spawn(async move {
            if let Err(e) = oj_server::tls::serve_router(listener, router, None).await {
                panic!("dev server stopped serving: {e:#}");
            }
        });
        Listening { addr, task }
    }

    /// `ws` that returns once the server side of the socket is subscribed
    /// to the HMR broadcast. The handshake completes before the upgrade
    /// callback runs, so a frame sent right after connect could otherwise
    /// miss the socket.
    pub async fn ws(&self, addr: SocketAddr, path: &str, protocol: Option<&str>) -> (Ws, Response) {
        let before = self.built.reload_tx.receiver_count();
        let conn = ws(addr, path, protocol).await;
        tokio::time::timeout(WAIT, async {
            while self.built.reload_tx.receiver_count() <= before {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("socket at {path} never subscribed to the broadcast"));
        conn
    }
}

/// Dials `ws://127.0.0.1:<port><path>`, offering `protocol` the way a
/// browser's `new WebSocket(url, [protocol])` does. The Host header is
/// the loopback IP, which the host check allows. No Origin is sent, so the
/// ws token check does not apply.
pub async fn ws(addr: SocketAddr, path: &str, protocol: Option<&str>) -> (Ws, Response) {
    let mut req = format!("ws://{addr}{path}").into_client_request().unwrap();
    if let Some(p) = protocol {
        req.headers_mut()
            .insert("sec-websocket-protocol", HeaderValue::from_str(p).unwrap());
    }
    tokio::time::timeout(WAIT, tokio_tungstenite::connect_async(req))
        .await
        .unwrap_or_else(|_| panic!("ws {path} did not open"))
        .unwrap_or_else(|e| panic!("ws {path} failed to open: {e}"))
}

/// The subprotocol the server echoed, if any.
pub fn echoed_protocol(res: &Response) -> Option<&str> {
    res.headers()
        .get("sec-websocket-protocol")
        .and_then(|v| v.to_str().ok())
}

/// The next text frame within `wait`, or None on timeout. Panics if the
/// socket closes first.
pub async fn next_text_within(ws: &mut Ws, wait: Duration) -> Option<String> {
    tokio::time::timeout(wait, async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(t))) => return t.as_str().to_owned(),
                Some(Ok(Message::Close(_))) | None => panic!("ws closed while waiting for a frame"),
                Some(Ok(_)) => {}
                Some(Err(e)) => panic!("ws read failed: {e}"),
            }
        }
    })
    .await
    .ok()
}

/// The next text frame, parsed; panics after `WAIT`.
pub async fn next_frame(ws: &mut Ws) -> serde_json::Value {
    let text = next_text_within(ws, WAIT)
        .await
        .expect("timed out waiting for a ws frame");
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("frame is not JSON ({e}): {text}"))
}

/// The first frame `pred` accepts, skipping the rest; panics after `WAIT`
/// with what was skipped.
pub async fn frame_matching(
    ws: &mut Ws,
    label: &str,
    pred: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Some(text) = next_text_within(ws, left).await else {
            panic!("timed out waiting for {label}, got: {}", seen.join(" | "));
        };
        if let Ok(frame) = serde_json::from_str::<serde_json::Value>(&text) {
            if pred(&frame) {
                return frame;
            }
        }
        seen.push(text);
    }
}

/// The data of the next custom `event` frame.
pub async fn custom_event(ws: &mut Ws, event: &str) -> serde_json::Value {
    frame_matching(ws, event, |f| f["type"] == "custom" && f["event"] == event).await["data"]
        .clone()
}

/// Sends a Vite client custom event (`import.meta.hot.send`'s wire shape).
pub async fn send_custom(ws: &mut Ws, event: &str, data: serde_json::Value) {
    let msg = serde_json::json!({ "type": "custom", "event": event, "data": data });
    ws.send(Message::text(msg.to_string())).await.unwrap();
}

pub fn is_reload(frame: &serde_json::Value) -> bool {
    frame["type"] == "full-reload" || frame["type"] == "update"
}

/// Closes `ws` and waits for the server to drop the connection. The server
/// unsubscribes from the broadcast before its socket goes, so a socket
/// opened after this counts subscribers from a settled number.
pub async fn close(mut ws: Ws) {
    let _ = ws.close(None).await;
    tokio::time::timeout(WAIT, async { while ws.next().await.is_some() {} })
        .await
        .expect("server did not close the socket");
}
