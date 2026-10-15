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

#[tokio::test(flavor = "multi_thread")]
async fn a_file_read_in_a_loop_never_holds_an_edit_back() {
    // inotify reports every open of a watched file; an editor, a language
    // server or a poller reading one in a loop must not keep the settle
    // window open forever (chokidar never surfaces reads at all).
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"watch-reads","version":"1.0.0"}"#,
        ),
        ("src/main.js", "document.title = \"v1\";\n"),
        ("src/polled.js", "export const polled = 1;\n"),
        ("index.html", INDEX_HTML),
    ]);
    let server = app.serve().await;
    server.text("/src/main.js").await;
    let mut rx = server.built.reload_tx.subscribe();

    let polled = app.root.join("src/polled.js");
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader = {
        let stop = std::sync::Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = std::fs::read(&polled);
                std::thread::sleep(Duration::from_millis(5));
            }
        })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    app.write(&[("src/main.js", "document.title = \"v2\";\n")]);
    let reload = next_reload(&mut rx, Duration::from_secs(10)).await;
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    reader.join().unwrap();
    assert!(
        reload.is_some(),
        "an edit made while another watched file is read in a loop reaches the browser"
    );
}
