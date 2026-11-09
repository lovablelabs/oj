// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Vite's client dials the origin root with the vite-hmr subprotocol while
//! oj's own socket lives at `/__ws`, so a root dial must still upgrade, echo
//! the subprotocol, send `connected` first and carry reloads; its vite-ping
//! liveness probe is accepted, then closed by the server.

use futures_util::StreamExt;
use tokio_tungstenite::tungstenite::Message;

use crate::listen::{
    close, echoed_protocol, frame_matching, is_reload, next_frame, next_text_within, ws, WAIT,
};
use crate::{App, INDEX_HTML};

#[tokio::test(flavor = "multi_thread")]
async fn root_vite_hmr_socket_connects_and_carries_reloads() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"vite-hmr-app","version":"1.0.0"}"#,
        ),
        ("src/main.js", "document.title = \"v1\";\n"),
        ("index.html", INDEX_HTML),
    ]);
    let server = app.serve().await;
    let listening = server.listen().await;

    let (mut socket, res) = server.ws(listening.addr, "/", Some("vite-hmr")).await;
    assert_eq!(
        echoed_protocol(&res),
        Some("vite-hmr"),
        "server echoes the vite-hmr subprotocol"
    );
    assert_eq!(
        next_frame(&mut socket).await["type"],
        "connected",
        "connected is the first frame"
    );

    // Seed the module into the graph, then edit it; the reload frame must
    // reach the root-dialed client.
    server.text("/src/main.js").await;
    app.write(&[("src/main.js", "document.title = \"v2\";\n")]);
    frame_matching(&mut socket, "reload frame after edit", is_reload).await;
    close(socket).await;

    // Not a broadcast subscriber, so the plain dial.
    let (mut ping, res) = ws(listening.addr, "/", Some("vite-ping")).await;
    assert_eq!(
        echoed_protocol(&res),
        Some("vite-ping"),
        "vite-ping socket opened"
    );
    let closed = tokio::time::timeout(WAIT, async {
        loop {
            match ping.next().await {
                Some(Ok(Message::Close(_))) | None => return,
                Some(Ok(Message::Text(t))) => panic!("vite-ping got a frame: {}", t.as_str()),
                Some(Ok(_)) => {}
                Some(Err(e)) => panic!("vite-ping socket errored: {e}"),
            }
        }
    })
    .await;
    assert!(
        closed.is_ok(),
        "vite-ping socket opened then was closed by the server"
    );
}

/// The socket pings each `OJ_HMR_PING_MS` with Vite's own `ping` payload,
/// which Vite's client and oj's client.js both ignore, so a client watching
/// the socket (the editor's, beside the page) hears from a healthy one even
/// when nothing changes. No interval, no pings.
#[tokio::test(flavor = "multi_thread")]
async fn root_vite_hmr_socket_pings_each_interval_and_never_without_one() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"vite-hmr-ping-app","version":"1.0.0"}"#,
        ),
        ("src/main.js", "document.title = \"v1\";\n"),
        ("index.html", INDEX_HTML),
    ]);
    let server = app.serve().await;
    let listening = server.listen().await;

    server
        .built
        .set_hmr_ping(Some(std::time::Duration::from_millis(100)));
    let (mut socket, _) = server.ws(listening.addr, "/", Some("vite-hmr")).await;
    assert_eq!(next_frame(&mut socket).await["type"], "connected");
    let started = std::time::Instant::now();
    for _ in 0..3 {
        let frame = frame_matching(&mut socket, "a ping", |f| f["type"] == "ping").await;
        assert_eq!(frame, serde_json::json!({ "type": "ping" }));
    }
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(250),
        "three pings came in {:?}: a burst, not an interval",
        started.elapsed()
    );
    close(socket).await;

    server.built.set_hmr_ping(None);
    let (mut quiet, _) = server.ws(listening.addr, "/", Some("vite-hmr")).await;
    assert_eq!(next_frame(&mut quiet).await["type"], "connected");
    let frame = next_text_within(&mut quiet, std::time::Duration::from_millis(400)).await;
    assert!(frame.is_none(), "a frame with no interval: {frame:?}");
    close(quiet).await;
}

/// A client end that stops answering the protocol's ping (it reads nothing:
/// its connection is gone) is closed after the deadline, with the reason;
/// a browser answers by itself, so a page paused in a debugger stays.
#[tokio::test(flavor = "multi_thread")]
async fn root_vite_hmr_socket_closes_a_client_that_stops_answering() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"vite-hmr-deadline-app","version":"1.0.0"}"#,
        ),
        ("src/main.js", "document.title = \"v1\";\n"),
        ("index.html", INDEX_HTML),
    ]);
    let server = app.serve().await;
    let listening = server.listen().await;
    let ping = std::time::Duration::from_millis(100);
    server.built.set_hmr_ping(Some(ping));
    let (mut socket, _) = server.ws(listening.addr, "/", Some("vite-hmr")).await;
    tokio::time::sleep(oj_server::silent_deadline(ping) + ping * 2).await;
    let closed = tokio::time::timeout(WAIT, async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Close(frame))) => return frame,
                None | Some(Err(_)) => return None,
                Some(Ok(_)) => {}
            }
        }
    })
    .await
    .expect("the silent client's socket stayed open");
    // The reason, when the close frame is read: answering the pings queued
    // while it read nothing, the client writes to a socket the server
    // already closed, and on Linux the reset discards the unread close.
    if let Some(frame) = closed {
        assert_eq!(frame.reason.as_str(), "oj: ping timeout");
    }
}
