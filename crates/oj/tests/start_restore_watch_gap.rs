// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A source file saved after the client bundle is restored from the store
//! and before the dev server's watcher is up is rebuilt into the client. The
//! restore checks its key against the sources as they are when it runs; a
//! save after that reached no watcher event, so the pages kept running the
//! restored bundle while the server rendered the saved file.
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
use std::time::{Duration, Instant};

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

/// Polls until the client holds `marker`, a minute at most.
fn wait_client_holds(dev: &Server, marker: &str) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !client_holds(dev, marker) {
        assert!(
            Instant::now() < deadline,
            "the client kept the restored bundle, without {marker:?}\n{}",
            dev.output()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn a_route_saved_between_the_restore_and_the_watcher_reaches_the_client() {
    let Some(app) = app("restore-watch-gap") else {
        return;
    };
    let root = &app.0;
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

    // The first start caches the client bundle.
    {
        let dev = Server::oj_dev(root);
        assert!(
            dev.logs("client bundle cached"),
            "the first start caches its bundle\n{}",
            dev.output()
        );
        assert!(client_holds(&dev, "about-before-restart"));
    }

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
        wait_client_holds(&dev, "about-saved-after-restore");
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
    wait_client_holds(&dev, "note-added-after-restore");
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
