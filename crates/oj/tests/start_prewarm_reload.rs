// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! An edit saved while the Start dev server warms its engine reloads the
//! page once, from the watcher's rebuild, at the editor's flush.
//!
//! The boot's revalidation drops the modules edited since the warm render
//! read them. The watcher thread scans the app before it reads its first
//! event, seconds on a slow disk, and an edit saved meanwhile queues on its
//! feed behind the scan; the revalidation, finishing first, used to find the
//! edit's module stale, see no change the watcher had taken, and reload the
//! page for it itself: past the editor's HMR gate, since the hold is taken
//! at the watcher's event, and a second time once the rebuild ended. It now
//! waits for the watcher to catch up with its feed before deciding that
//! nothing else reloads for the edit. Both delays are stretched here with
//! the test knobs `OJ_START_PREWARM_DELAY_MS` (the gap between the warm
//! render and the revalidation) and `OJ_WATCH_SCAN_DELAY_MS` (both
//! watchers' boot scans).

#[allow(dead_code)]
mod start_app;

use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use start_app::{app, Server, WAIT};
use tokio_tungstenite::tungstenite::Message;

const GATE: &[(&str, &str)] = &[("OJ_HMR_GATE", "1"), ("OJ_HMR_FULL_RELOAD", "true")];
const SLOW_BOOT: &[(&str, &str)] = &[
    ("OJ_START_PREWARM_DELAY_MS", "2000"),
    ("OJ_WATCH_SCAN_DELAY_MS", "6000"),
];

/// The frames that reload a page, counted off the Start live-reload socket.
struct Reloads {
    count: Arc<AtomicUsize>,
    socket: tokio::task::JoinHandle<()>,
}

async fn listen(dev: &Server) -> Reloads {
    let url = format!("ws://127.0.0.1:{}/@oj-start/hmr", dev.port);
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.expect("dial");
    let count = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&count);
    let socket = tokio::spawn(async move {
        while let Some(Ok(msg)) = ws.next().await {
            if let Message::Text(text) = msg {
                if text == "reload" || text == r#"{"type":"oj:behind"}"# {
                    seen.fetch_add(1, SeqCst);
                }
            }
        }
    });
    Reloads { count, socket }
}

fn output_has(dev: &Server, needle: &str, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while !dev.output().contains(needle) {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    true
}

#[tokio::test(flavor = "multi_thread")]
async fn an_edit_during_the_warm_up_reloads_once_at_the_flush() {
    let Some(app) = app("prewarm-reload") else {
        return;
    };
    let root = app.0.clone();
    let env: Vec<(&str, &str)> = GATE.iter().chain(SLOW_BOOT).copied().collect();
    let dev = Server::oj_dev_with(&root, &env);
    let reloads = listen(&dev).await;
    // The engine has rendered the routes (this request, if not the warm
    // render yet), so an edit from here on makes a module stale.
    let (status, _) = dev.request("GET", "/", &[]);
    assert_eq!(status, 200, "{}", dev.output());
    let about = root.join("src/routes/about.tsx");
    let original = std::fs::read_to_string(&about).unwrap();
    std::fs::write(
        &about,
        format!("{original}\n// edited during the warm-up\n"),
    )
    .unwrap();

    // The watcher, done scanning, takes the edit: the gate holds its reload.
    assert!(
        output_has(&dev, "reload held for the editor's flush", WAIT),
        "the watcher's rebuild did not hold a reload\n{}",
        dev.output()
    );
    // The boot's revalidation found the edit's module stale meanwhile, and
    // sent nothing: the reload is the rebuild's, held for the flush.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        reloads.count.load(SeqCst),
        0,
        "a reload reached the page before the flush\n{}",
        dev.output()
    );

    let (status, body) = dev.request("POST", "/__hmr_flush", &[]);
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains("\"reload\":true"),
        "the flush released no reload: {body}"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while reloads.count.load(SeqCst) == 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        reloads.count.load(SeqCst),
        1,
        "expected exactly one reload, at the flush\n{}",
        dev.output()
    );
    reloads.socket.abort();
}
