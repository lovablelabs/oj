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

/// A plugin's hot-update hook that never settles parks that edit's HMR, not
/// the watcher: a `.env` edit after it still restarts, as Vite keeps
/// handling chokidar events while a hook awaits.
#[tokio::test(flavor = "multi_thread")]
async fn a_hung_hot_update_hook_never_holds_a_restart_back() {
    let app = App::new(&[
        ("package.json", r#"{"name":"hung-hook","version":"1.0.0"}"#),
        (
            "oj.plugins.mjs",
            "export default [{ name: \"hang\", handleHotUpdate(ctx) {\n  if (ctx.file.endsWith(\"/src/main.js\")) return new Promise(() => {});\n} }];\n",
        ),
        ("src/main.js", "document.title = \"v1\";\n"),
        ("index.html", INDEX_HTML),
    ]);
    let server = app.serve().await;
    server.text("/src/main.js").await;
    app.write(&[("src/main.js", "document.title = \"v2\";\n")]);
    // Let that batch reach the hook before the restart trigger lands.
    tokio::time::sleep(Duration::from_millis(500)).await;
    app.write(&[(".env", "VITE_X=1\n")]);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut restarts = Vec::new();
    while restarts.is_empty() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
        restarts = crate::take_restarts_for(&app.root);
    }
    assert!(
        restarts.len() == 1 && restarts[0].ends_with("/.env changed"),
        "the .env edit never restarted: {restarts:?}"
    );
}

/// A hot-update hook that never settles for one file leaves every other
/// file's HMR alone: Vite handles each watcher event on its own
/// (`onFileChange(file).catch(...)`), never queued behind an earlier one.
#[tokio::test(flavor = "multi_thread")]
async fn a_hung_hot_update_hook_never_holds_another_files_update() {
    let app = App::new(&[
        ("package.json", r#"{"name":"hung-other","version":"1.0.0"}"#),
        (
            "oj.plugins.mjs",
            "export default [{ name: \"hang\", handleHotUpdate(ctx) {\n  if (ctx.file.endsWith(\"/src/a.js\")) return new Promise(() => {});\n} }];\n",
        ),
        ("src/main.js", "import \"./a.js\";\nimport \"./b.js\";\n"),
        ("src/a.js", "export const a = 1;\n"),
        ("src/b.js", "export const b = 1;\n"),
        ("index.html", INDEX_HTML),
    ]);
    let server = app.serve().await;
    for url in ["/src/main.js", "/src/a.js", "/src/b.js"] {
        server.text(url).await;
    }
    let mut rx = server.built.reload_tx.subscribe();
    app.write(&[("src/a.js", "export const a = 2;\n")]);
    // Let that batch reach the hook before the other edit lands.
    tokio::time::sleep(Duration::from_millis(500)).await;
    app.write(&[("src/b.js", "export const b = 2;\n")]);
    let frame = next_reload(&mut rx, Duration::from_secs(10)).await;
    assert!(
        frame.as_deref().is_some_and(|f| f.contains("/src/b.js")),
        "b.js's update never arrived behind a.js's hung hook: {frame:?}"
    );
}
