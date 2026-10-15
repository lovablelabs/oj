// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A file something keeps reading must not starve the watcher. On Linux,
//! inotify reports every open as an event, and the batch loops restart their
//! settle window on each one, so an edit made while a poller reads a file
//! under the root was never handed off: no HMR frame, and under Start no
//! rebundle or route tree regeneration.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::{App, INDEX_HTML};

#[tokio::test(flavor = "multi_thread")]
async fn an_edit_lands_while_a_file_is_read_in_a_loop() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"watch-reads","version":"1.0.0"}"#,
        ),
        ("src/main.js", "document.title = \"v1\";\n"),
        ("src/polled.txt", "read me\n"),
        ("index.html", INDEX_HTML),
    ]);
    let server = app.serve().await;
    server.text("/src/main.js").await;
    let mut rx = server.built.reload_tx.subscribe();

    // Faster than the settle window, like a test or editor polling a file.
    let stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let (stop, polled) = (Arc::clone(&stop), app.root.join("src/polled.txt"));
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let _ = std::fs::read(&polled);
                std::thread::sleep(Duration::from_millis(2));
            }
        })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    app.write(&[("src/main.js", "document.title = \"v2\";\n")]);

    let got = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match rx.recv().await {
                Ok(frame) => {
                    let ty = serde_json::from_str::<serde_json::Value>(&frame)
                        .ok()
                        .and_then(|f| f["type"].as_str().map(str::to_string));
                    if matches!(ty.as_deref(), Some("full-reload" | "update")) {
                        return;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(err) => panic!("reload channel closed: {err}"),
            }
        }
    })
    .await;
    stop.store(true, Ordering::Relaxed);
    reader.join().unwrap();
    assert!(
        got.is_ok(),
        "the edit never reached HMR while a file was being read"
    );
}
