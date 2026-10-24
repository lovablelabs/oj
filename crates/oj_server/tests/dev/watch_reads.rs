// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A file something keeps reading must not starve the watcher. On Linux,
//! inotify reports every open as an event, and the batch loops restart their
//! settle window on each one, so an edit made while a poller reads a file
//! under the root was never handed off: no HMR frame, and under Start no
//! rebundle or route tree regeneration. A file written nonstop under
//! `server.watch.ignored` is the same hazard: its writes must not open or
//! extend a window.

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
    assert!(
        edit_lands_while(&app, "src/polled.txt", |p| {
            let _ = std::fs::read(p);
        })
        .await,
        "the edit never reached HMR while a file was being read"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_edit_lands_while_an_ignored_file_is_written_in_a_loop() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"watch-ignored-writes","version":"1.0.0"}"#,
        ),
        (
            "vite.config.js",
            "export default { server: { watch: { ignored: [\"**/logs/**\"] } } };\n",
        ),
        ("src/main.js", "document.title = \"v1\";\n"),
        ("logs/out.log", ""),
        ("index.html", INDEX_HTML),
    ]);
    assert!(
        edit_lands_while(&app, "logs/out.log", append_line).await,
        "the edit never reached HMR while an ignored file was being written"
    );
}

fn append_line(path: &std::path::Path) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(path) {
        let _ = f.write_all(b"x\n");
    }
}

/// Edits src/main.js while `touch` hits `busy` faster than the settle
/// window, as a test, editor or log writer would, and reports whether the
/// edit still reached HMR.
async fn edit_lands_while(app: &App, busy: &str, touch: fn(&std::path::Path)) -> bool {
    let server = app.serve().await;
    server.text("/src/main.js").await;
    let mut rx = server.built.reload_tx.subscribe();

    let stop = Arc::new(AtomicBool::new(false));
    let worker = {
        let (stop, busy) = (Arc::clone(&stop), app.root.join(busy));
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                touch(&busy);
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
    worker.join().unwrap();
    got.is_ok()
}
