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

/// One app's restart is that app's test's to claim: another server dropping
/// meanwhile neither fails on it nor takes it away.
#[tokio::test(flavor = "multi_thread")]
async fn a_restart_belongs_to_its_own_apps_server() {
    let files = [
        (
            "package.json",
            r#"{"name":"own-restart","version":"1.0.0"}"#,
        ),
        ("src/main.js", "document.title = \"v1\";\n"),
        ("index.html", INDEX_HTML),
    ];
    let (ours, other) = (App::new(&files), App::new(&files));
    let _server = ours.serve().await;
    let bystander = other.serve().await;
    ours.write(&[(".env", "VITE_X=1\n")]);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let restarted = crate::RESTARTS
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.ends_with("/.env changed"));
        if restarted {
            break;
        }
    }
    drop(bystander);
    let restarts = crate::take_restarts_for(&ours.root);
    assert!(
        restarts.len() == 1 && restarts[0].ends_with("/.env changed"),
        "the restart was not left for its own app: {restarts:?}"
    );
}

/// Watching a directory outside the root (an `@import`ed sheet's) never drops
/// a change under the root. Vite's chokidar opens an FSEvents stream per
/// watched path, leaving the running ones alone; restarting the one stream
/// to add a path lost whatever landed meanwhile. Each write here is a new
/// file, so no two coalesce into one event.
#[tokio::test(flavor = "multi_thread")]
async fn adding_an_outside_watch_drops_no_root_change() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"outside-watch","version":"1.0.0"}"#,
        ),
        ("index.html", INDEX_HTML),
        ("src/main.js", "window.__ok = 1;\n"),
    ]);
    let name = format!("{}-theme", app.root.file_name().unwrap().to_string_lossy());
    let outside = app.root.with_file_name(&name);
    std::fs::create_dir_all(&outside).unwrap();
    let theme = outside.join("theme.css");
    std::fs::write(&theme, ".seed { margin: 0 }\n").unwrap();
    app.write(&[(
        "src/app.css",
        &format!("@import \"../../{name}/theme.css\";\n"),
    )]);
    let server = app.serve().await;
    let seen = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
    let feed = server.built.watch_feed.subscribe();
    let collector = std::sync::Arc::clone(&seen);
    std::thread::spawn(move || {
        while let Ok(ev) = feed.recv() {
            collector.lock().unwrap().extend(ev.paths);
        }
    });

    // Keep creating root files from before the outside watch is asked for
    // until a while after it delivers.
    let (root, theme_w, seen_w) = (
        app.root.clone(),
        theme.clone(),
        std::sync::Arc::clone(&seen),
    );
    let writer = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(90);
        let (mut written, mut after_live) = (Vec::new(), None);
        for i in 0.. {
            let file = root.join(format!("src/t{i}.txt"));
            std::fs::write(&file, "x").unwrap();
            written.push(file);
            std::fs::write(&theme_w, format!(".e{i} {{ margin: 0 }}\n")).unwrap();
            if after_live.is_none() && seen_w.lock().unwrap().contains(&theme_w) {
                after_live = Some(i);
            }
            if after_live.is_some_and(|live| i >= live + 20) || std::time::Instant::now() > deadline
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        (written, after_live.is_some())
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    // Serving the sheet asks for its outside dependency's directory.
    server.text("/src/app.css?direct").await;
    let (written, live) = tokio::task::spawn_blocking(move || writer.join().unwrap())
        .await
        .unwrap();
    assert!(live, "the outside watch never delivered");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let missing: Vec<_> = {
            let seen = seen.lock().unwrap();
            written
                .iter()
                .filter(|f| !seen.contains(*f))
                .cloned()
                .collect()
        };
        if missing.is_empty() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{} of {} root files never reached the watcher, e.g. {:?}",
            missing.len(),
            written.len(),
            missing.first()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let _ = std::fs::remove_dir_all(&outside);
}
