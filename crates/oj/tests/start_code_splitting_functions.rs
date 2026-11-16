// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A Start app's chunk grouping with functions in it: a
//! `build.rollupOptions.output.codeSplitting.groups[]` entry whose `name` and
//! `test` are functions, as rolldown takes them (`(moduleId, ctx) => ...`).
//! Vite hands them to rolldown in `vite build` and in its bundled dev; its
//! unbundled dev server never calls them, which is why such a config can
//! work there and break only where dev bundles. The functions live in the
//! plugin host, and the dev client bundle calls them through handles for as
//! long as the server runs: on the first bundle, on a rebundle after an
//! edit, and from a server that starts on the cache the first one left.
//! `output.manualChunks` as a function is the same thing to rolldown, a
//! group whose name it gives.
//!
//! The app is the Start fixture (see `start_app`) with a lazily imported
//! module, so the group has a chunk to name. None of this respawns the
//! plugin host's engine: that a handle outlives it is covered where the
//! engine can be wedged (`oj_server`'s `vite_values_tests`).

#[allow(dead_code)]
mod start_app;

use std::path::Path;
use std::time::{Duration, Instant};

use start_app::{app, Server};

const ABOUT: &str = r#"import { createRoute } from "@tanstack/react-router";
import { useEffect, useState } from "react";

import { rootRoute } from "./__root";

export const aboutRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/about",
  component: About,
});

function About() {
  const [note, setNote] = useState("loading");
  useEffect(() => {
    import("../lib/lazy-note").then((m) => setNote(m.note));
  }, []);
  return (
    <main>
      <h1 className="fixture-heading">about-page-marker</h1>
      <p id="note">{note}</p>
    </main>
  );
}
"#;

/// The grouping, written as the report had it: `rollupOptions` (Vite's older
/// name for `rolldownOptions`), a `name` function that puts the lazy note in
/// a chunk of its own and leaves every other module to rolldown, and a
/// `test` function over the id.
const GROUPS: &str = r#"  build: {
    rollupOptions: {
      output: {
        codeSplitting: {
          groups: [
            {
              name: (id) => (id.includes("/lib/lazy-note") ? "fixture-note" : undefined),
              test: (id) => !id.includes("node_modules"),
            },
          ],
        },
      },
    },
  },
"#;

fn write_note(root: &Path, text: &str) {
    std::fs::write(
        root.join("src/lib/lazy-note.ts"),
        format!("export const note = {text:?};\n"),
    )
    .unwrap();
}

/// Rollup's older way to the same grouping: rolldown makes one group of the
/// function, when no `codeSplitting` object is there.
const MANUAL_CHUNKS: &str = r#"  build: {
    rollupOptions: {
      output: {
        manualChunks: (id, { getModuleInfo }) =>
          id.includes("/lib/lazy-note") && getModuleInfo(id) ? "fixture-note" : undefined,
      },
    },
  },
"#;

/// The fixture with `grouping` in its config and the lazily imported note.
fn prepared(name: &str, grouping: &str) -> Option<start_app::App> {
    let app = app(name)?;
    let config_file = app.0.join("vite.config.ts");
    let config = std::fs::read_to_string(&config_file).unwrap();
    let marker = "export default defineConfig({\n";
    assert!(
        config.contains(marker),
        "the fixture config's shape changed"
    );
    std::fs::write(
        &config_file,
        config.replacen(marker, &format!("{marker}{grouping}"), 1),
    )
    .unwrap();
    std::fs::write(app.0.join("src/routes/about.tsx"), ABOUT).unwrap();
    write_note(&app.0, "lazy-note-one");
    Some(app)
}

/// The chunk files of the latest client bundle (`client-chunks.json`).
fn chunk_files(root: &Path) -> Vec<String> {
    let path = root.join(".oj-cache/v1/start/client-chunks.json");
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    json["files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| f["name"].as_str().map(str::to_string))
        .collect()
}

/// The note's chunk, named by the group's function, once the bundle has one
/// other than `not`; the server's output when it never does.
fn wait_for_group_chunk(server: &Server, root: &Path, not: Option<&str>) -> String {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let files = chunk_files(root);
        if let Some(name) = files.iter().find(|f| f.starts_with("fixture-note")) {
            if Some(name.as_str()) != not {
                return name.clone();
            }
        }
        assert!(
            !server.output().contains("no config function"),
            "the group's function was lost\n{}",
            server.output()
        );
        assert!(
            Instant::now() < deadline,
            "no chunk named by the group's function in {files:?}\n{}",
            server.output()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The chunk by name, as the page loads it, once the server serves it. The
/// name comes from the manifest the bundle writes to the cache, and the bundle
/// writes that before the server takes the bundle as the one it serves, so
/// for a moment after an edit the server answers 404 for a name the file
/// already has. The answer after 30 seconds is returned as it stands.
fn chunk(server: &Server, name: &str) -> (u16, String) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let answer = server.request("GET", &format!("/@oj-start/{name}"), &[]);
        if answer.0 != 404 || Instant::now() >= deadline {
            return answer;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The group's `name` and `test` functions run on the first bundle, on the
/// rebundle an edit brings, and from a server started on the warm cache the
/// first one left: each bundle has the note in the chunk the function names,
/// and none fails on a lost function.
#[test]
fn group_functions_name_the_chunk_on_every_bundle_and_across_a_restart() {
    let Some(app) = prepared("code-splitting-functions", GROUPS) else {
        return;
    };
    let root = app.0.clone();
    let dev = Server::oj_dev(&root);
    let first = wait_for_group_chunk(&dev, &root, None);
    let (status, body) = chunk(&dev, &first);
    assert!(
        status == 200 && body.contains("lazy-note-one"),
        "the named chunk before any edit: {status} {body:?}\n{}",
        dev.output()
    );
    let (status, body) = dev.request("GET", "/about", &[]);
    assert!(
        status == 200 && body.contains("about-page-marker"),
        "the about page: {status}\n{}",
        dev.output()
    );

    // An edit to the note: the rebundle calls the functions again.
    write_note(&root, "lazy-note-two");
    let second = wait_for_group_chunk(&dev, &root, Some(&first));
    let (status, body) = chunk(&dev, &second);
    assert!(
        status == 200 && body.contains("lazy-note-two"),
        "the named chunk after the edit: {status} {body:?}\n{}",
        dev.output()
    );
    assert!(
        !dev.output().contains("no config function")
            && !dev.output().contains("UNHANDLEABLE")
            && !dev.output().contains("steps aside"),
        "the bundle lost the group's function\n{}",
        dev.output()
    );
    drop(dev);

    // A second server on the cache the first left, over a note edited while
    // none ran: the bundle it has to make reads the config afresh and calls
    // the functions it finds there. The chunk list on disk is the first
    // server's until then, so the chunk waited for is one it never named.
    write_note(&root, "lazy-note-three");
    let again = Server::oj_dev(&root);
    let (status, body) = again.request("GET", "/about", &[]);
    assert!(
        status == 200 && body.contains("about-page-marker"),
        "the about page on the warm cache: {status}\n{}",
        again.output()
    );
    let third = wait_for_group_chunk(&again, &root, Some(&second));
    assert_ne!(third, first, "a chunk of the edited note");
    let (status, body) = chunk(&again, &third);
    assert!(
        status == 200 && body.contains("lazy-note-three"),
        "the named chunk on the warm cache: {status} {body:?}\n{}",
        again.output()
    );
    assert!(
        !again.output().contains("no config function") && !again.output().contains("steps aside"),
        "the bundle lost the group's function\n{}",
        again.output()
    );
}

/// `output.manualChunks` as a function names the note's chunk as the group
/// does, on the first bundle and on the rebundle an edit brings, and is
/// called with `getModuleInfo` answering for the module.
#[test]
fn a_manual_chunks_function_names_the_chunk_as_a_group_does() {
    let Some(app) = prepared("code-splitting-manual-chunks", MANUAL_CHUNKS) else {
        return;
    };
    let root = app.0.clone();
    let dev = Server::oj_dev(&root);
    let first = wait_for_group_chunk(&dev, &root, None);
    let (status, body) = chunk(&dev, &first);
    assert!(
        status == 200 && body.contains("lazy-note-one"),
        "the named chunk before any edit: {status} {body:?}\n{}",
        dev.output()
    );
    write_note(&root, "lazy-note-two");
    let second = wait_for_group_chunk(&dev, &root, Some(&first));
    let (status, body) = chunk(&dev, &second);
    assert!(
        status == 200 && body.contains("lazy-note-two"),
        "the named chunk after the edit: {status} {body:?}\n{}",
        dev.output()
    );
    assert!(
        !dev.output().contains("steps aside") && !dev.output().contains("UNHANDLEABLE"),
        "the function could not be called\n{}",
        dev.output()
    );
}
