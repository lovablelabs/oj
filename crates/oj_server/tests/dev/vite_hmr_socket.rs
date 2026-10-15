// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Vite's client dials the origin root with the vite-hmr subprotocol while
//! oj's own socket lives at `/__ws`, so a root dial must still upgrade, echo
//! the subprotocol, send `connected` first and carry reloads; its vite-ping
//! liveness probe is accepted, then closed by the server.

use futures_util::StreamExt;
use tokio_tungstenite::tungstenite::Message;

use crate::listen::{close, echoed_protocol, frame_matching, is_reload, next_frame, ws, WAIT};
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
