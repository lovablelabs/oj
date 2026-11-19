// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A source file saved after the client bundle is restored from the store
//! and before the dev server's watcher is up reaches the client from the
//! first page on. The restore checks its key against the sources as they
//! are when it runs; a save after that reached no watcher event, so the
//! pages kept running the restored bundle while the server rendered the
//! saved file.
//!
//! A file created then under one of the bundle's `import.meta.glob`s is
//! rebuilt in the same way.
//!
//! The app is the Start fixture (see `start_app`) with a plugin whose
//! `configureServer` waits, when the test asks, until the test has written
//! its files, which holds the server's start (and its watcher) past them.

#[allow(dead_code)]
mod start_app;

use std::collections::BTreeSet;
use std::path::Path;

use start_app::{app, Server};

const HOLD_PLUGIN: &str = r#"
function holdServerPlugin() {
  return {
    name: "hold-configure-server",
    async configureServer() {
      const release = process.env.OJ_TEST_HOLD_CONFIGURE_SERVER;
      if (!release) return;
      const { existsSync } = await import("node:fs");
      for (let i = 0; i < 1200 && !existsSync(release); i++) {
        await new Promise((resolve) => setTimeout(resolve, 50));
      }
    },
  };
}
"#;

fn about(marker: &str) -> String {
    format!(
        r#"import {{ createRoute }} from "@tanstack/react-router";

import {{ rootRoute }} from "./__root";

const notes = import.meta.glob("../notes/*.ts", {{ eager: true, import: "note" }});

export const aboutRoute = createRoute({{
  getParentRoute: () => rootRoute,
  path: "/about",
  component: About,
}});

function About() {{
  return <p id="about">{marker} {{Object.values(notes).join(",")}}</p>;
}}
"#
    )
}

fn write_note(root: &Path, name: &str, text: &str) {
    let dir = root.join("src/notes");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{name}.ts")),
        format!("export const note = {text:?};\n"),
    )
    .unwrap();
}

fn write_about(root: &Path, marker: &str) {
    std::fs::write(root.join("src/routes/about.tsx"), about(marker)).unwrap();
}

/// Whether any chunk the served client entry reaches holds `marker`.
fn client_holds(dev: &Server, marker: &str) -> bool {
    let chunk_name = regex::Regex::new(r"[A-Za-z0-9_.-]+-[A-Za-z0-9_-]{8}\.js").unwrap();
    let mut seen = BTreeSet::new();
    let mut todo = vec!["client-entry.js".to_string()];
    while let Some(name) = todo.pop() {
        if !seen.insert(name.clone()) {
            continue;
        }
        let (status, body) = dev.request("GET", &format!("/@oj-start/{name}"), &[]);
        if status != 200 {
            continue;
        }
        if body.contains(marker) {
            return true;
        }
        todo.extend(chunk_name.find_iter(&body).map(|m| m.as_str().to_string()));
    }
    false
}

/// Asserts the client holds `marker` right after the page that loads it,
/// with no wait: a page loaded then runs it.
fn assert_client_holds(dev: &Server, marker: &str) {
    assert!(
        client_holds(dev, marker),
        "the first page got the restored client, without {marker:?}\n{}",
        dev.output()
    );
}

#[test]
fn a_route_saved_between_the_restore_and_the_watcher_reaches_the_client() {
    let Some(app) = app("restore-watch-gap") else {
        return;
    };
    let root = &app.0;
    cached_start(root);

    // The second restores it, and the route is saved while the server is
    // still held in `configureServer`, which the test then releases.
    {
        let dev = held_start(root, "a", || write_about(root, "about-saved-after-restore"));
        let (status, page) = dev.request("GET", "/about", &[]);
        assert!(
            status == 200 && page.contains("about-saved-after-restore"),
            "the server renders the saved route: {status} {page:?}\n{}",
            dev.output()
        );
        assert_client_holds(&dev, "about-saved-after-restore");
    }

    // The third restores that bundle, and a note is created under its glob,
    // the one change: no file the bundle read moved.
    let dev = held_start(root, "b", || {
        write_note(root, "two", "note-added-after-restore")
    });
    let (status, page) = dev.request("GET", "/about", &[]);
    assert!(
        status == 200 && page.contains("note-added-after-restore"),
        "the server renders the new note: {status} {page:?}\n{}",
        dev.output()
    );
    assert_client_holds(&dev, "note-added-after-restore");
}

/// The fixture with the holding plugin, the route and a note, and a first
/// start that caches the client bundle.
fn cached_start(root: &Path) {
    let config = root.join("vite.config.ts");
    let source = std::fs::read_to_string(&config).unwrap();
    let source = source.replacen(
        "  plugins: [\n",
        "  plugins: [\n    holdServerPlugin(),\n",
        1,
    );
    std::fs::write(&config, format!("{source}{HOLD_PLUGIN}")).unwrap();
    write_about(root, "about-before-restart");
    write_note(root, "one", "note-one");
    let dev = Server::oj_dev(root);
    assert!(
        dev.logs("client bundle cached"),
        "the first start caches its bundle\n{}",
        dev.output()
    );
    assert!(client_holds(&dev, "about-before-restart"));
}

// A save that breaks the build fails the boot's rebuild; the first page is
// answered once it failed, not held until the documents' wait gives up.
#[test]
fn a_failed_boot_rebuild_still_answers_the_first_page() {
    let Some(app) = app("restore-watch-gap-broken") else {
        return;
    };
    let root = &app.0;
    cached_start(root);
    let dev = held_start(root, "broken", || {
        std::fs::write(root.join("src/routes/about.tsx"), "export const = ;\n").unwrap()
    });
    let asked = std::time::Instant::now();
    let (status, page) = dev.request("GET", "/about", &[]);
    assert!(
        asked.elapsed() < std::time::Duration::from_secs(30),
        "the first page waited {:?}\n{}",
        asked.elapsed(),
        dev.output()
    );
    assert_eq!(status, 500, "{page:?}\n{}", dev.output());
    let replay = error_replay(&dev);
    assert!(
        replay
            .as_deref()
            .is_some_and(|frame| frame.contains("about.tsx")),
        "the page's socket replays the build error: {replay:?}\n{}",
        dev.output()
    );
    assert!(
        dev.logs("client bundle failed"),
        "the boot's rebuild failed\n{}",
        dev.output()
    );
}

/// The error frame a page's socket (`/@oj-start/hmr`) gets on connect, if
/// one comes within ten seconds.
fn error_replay(dev: &Server) -> Option<String> {
    use futures_util::StreamExt;
    let url = format!("ws://127.0.0.1:{}/@oj-start/hmr", dev.port);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async move {
        let (mut socket, _) = tokio_tungstenite::connect_async(url).await.ok()?;
        let read = async {
            while let Some(Ok(message)) = socket.next().await {
                if let tokio_tungstenite::tungstenite::Message::Text(text) = message {
                    if text.contains("\"type\":\"error\"") {
                        return Some(text.to_string());
                    }
                }
            }
            None
        };
        tokio::time::timeout(std::time::Duration::from_secs(10), read)
            .await
            .ok()
            .flatten()
    })
}

// A fresh build right after the sources were written rebuilds nothing at boot.
#[test]
fn a_start_right_after_the_sources_were_written_rebuilds_nothing() {
    let Some(app) = app("restore-watch-gap-fresh") else {
        return;
    };
    let root = &app.0;
    let soon = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
    for dir in ["src", "styles"] {
        touch_tree(&root.join(dir), soon);
    }
    let dev = Server::oj_dev(root);
    let (status, _) = dev.request("GET", "/about", &[]);
    assert_eq!(status, 200, "{}", dev.output());
    std::thread::sleep(std::time::Duration::from_secs(1));
    assert!(
        !dev.output().contains("rebuilding for"),
        "the boot rebuilt files it had just built from\n{}",
        dev.output()
    );
}

// A restore right after the sources were written (a checkout of a branch
// whose bundle is cached) rebuilds nothing at boot.
#[test]
fn a_restore_right_after_the_sources_were_written_rebuilds_nothing() {
    let Some(app) = app("restore-watch-gap-touched") else {
        return;
    };
    let root = &app.0;
    cached_start(root);
    let soon = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
    for dir in ["src", "styles"] {
        touch_tree(&root.join(dir), soon);
    }
    let dev = Server::oj_dev(root);
    assert!(
        dev.logs("client bundle restored from cache"),
        "{}",
        dev.output()
    );
    let asked = std::time::Instant::now();
    let (status, _) = dev.request("GET", "/about", &[]);
    assert_eq!(status, 200, "{}", dev.output());
    std::thread::sleep(std::time::Duration::from_secs(1));
    assert!(
        !dev.output().contains("rebuilding for"),
        "the boot rebuilt files its restore had verified (first page took {:?})\n{}",
        asked.elapsed(),
        dev.output()
    );
}

// Its twin: a same-size save keeping the mtime, during the start, reaches the
// first page through the restore's digest.
#[test]
fn a_same_size_save_inside_the_slack_after_a_restore_reaches_the_first_page() {
    let Some(app) = app("restore-watch-gap-same-size") else {
        return;
    };
    let root = &app.0;
    cached_start(root);
    let soon = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
    for dir in ["src", "styles"] {
        touch_tree(&root.join(dir), soon);
    }
    let marker = "about-saved-in-slack";
    assert_eq!(marker.len(), "about-before-restart".len());
    let dev = held_start(root, "same-size", || {
        write_about(root, marker);
        touch_tree(&root.join("src/routes"), soon);
    });
    let (status, page) = dev.request("GET", "/about", &[]);
    assert!(
        status == 200 && page.contains(marker),
        "the server renders the save: {status} {page:?}\n{}",
        dev.output()
    );
    assert_client_holds(&dev, marker);
}

fn touch_tree(dir: &Path, mtime: std::time::SystemTime) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            touch_tree(&path, mtime);
        } else {
            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(mtime))
                .unwrap();
        }
    }
}

/// A start that restores the client bundle and is held in `configureServer`
/// until `write` has run.
fn held_start(root: &Path, label: &str, write: impl FnOnce()) -> Server {
    let release = root.join(format!("release-configure-server-{label}"));
    Server::oj_dev_at(
        root,
        &[("OJ_TEST_HOLD_CONFIGURE_SERVER", release.to_str().unwrap())],
        "client bundle restored from cache",
        || {
            write();
            std::fs::write(&release, "").unwrap();
        },
    )
}
