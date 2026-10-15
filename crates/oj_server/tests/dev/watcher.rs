// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The watcher's lifecycle seams the other tests lean on: `watcher_ready`
//! means a single edit is seen, and `close_watcher` (Vite's server.close()
//! closing chokidar) means nothing under the root is seen after it.

use std::time::Duration;

use crate::{App, INDEX_HTML};

fn is_reload(frame: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(frame)
        .ok()
        .and_then(|v| v["type"].as_str().map(str::to_owned))
        .is_some_and(|t| t == "full-reload" || t == "update")
}

async fn next_reload(
    rx: &mut tokio::sync::broadcast::Receiver<String>,
    within: Duration,
) -> Option<String> {
    tokio::time::timeout(within, async {
        loop {
            match rx.recv().await {
                Ok(frame) if is_reload(&frame) => return frame,
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(err) => panic!("reload channel closed: {err}"),
            }
        }
    })
    .await
    .ok()
}

#[tokio::test(flavor = "multi_thread")]
async fn ready_watcher_sees_one_edit_and_a_closed_one_sees_none() {
    let app = App::new(&[
        ("package.json", r#"{"name":"watch-app","version":"1.0.0"}"#),
        ("src/main.js", "document.title = \"v1\";\n"),
        ("index.html", INDEX_HTML),
    ]);
    let server = app.serve().await;
    server.text("/src/main.js").await;
    let mut rx = server.built.reload_tx.subscribe();

    app.write(&[("src/main.js", "document.title = \"v2\";\n")]);
    assert!(
        next_reload(&mut rx, Duration::from_secs(10))
            .await
            .is_some(),
        "an edit after watcher_ready reaches the browser channel"
    );

    server.built.close_watcher();
    app.write(&[("src/main.js", "document.title = \"v3\";\n")]);
    let late = next_reload(&mut rx, Duration::from_secs(2)).await;
    assert!(late.is_none(), "closed watcher still delivered {late:?}");
}

/// Once the listener is bound the watcher covers the root, as Vite's watcher
/// exists before listen: a write made the moment the port opens is seen.
#[tokio::test(flavor = "multi_thread")]
async fn an_edit_right_after_the_listener_binds_is_seen() {
    let app = App::new(&[
        ("package.json", r#"{"name":"bind-app","version":"1.0.0"}"#),
        ("src/main.js", "document.title = \"v1\";\n"),
        ("index.html", INDEX_HTML),
    ]);
    let mut dev = app.dev();
    dev.port = Some(0);
    let built = dev.build_app().await.expect("dev server builds");
    let feed = built.watch_feed.subscribe();
    let (_listener, _port) = built.bind_listener().await.expect("listener binds");
    app.write(&[("src/late.js", "export {};\n")]);
    let late = app.root.join("src/late.js");
    let seen = tokio::task::spawn_blocking(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
            match feed.recv_timeout(left) {
                Ok(ev) if ev.paths.iter().any(|p| p.ends_with("src/late.js")) => return true,
                Ok(_) => {}
                Err(_) => return false,
            }
        }
        false
    })
    .await
    .unwrap();
    built.close_watcher();
    assert!(
        seen,
        "the write after bind never reached the watcher ({})",
        late.display()
    );
}
