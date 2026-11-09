// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A page open across a rebundle keeps loading its chunks. A rebundle renames
//! every chunk whose content changed (rolldown's `[name]-[hash]`), and a
//! document served before it still imports a lazy chunk by its old name when
//! the user first needs it. The dev server keeps serving the chunks of the
//! bundles before the current one, so that import loads the code the
//! document was built with instead of a 404 and a blank page.
//!
//! The app is the Start fixture (see `start_app`) with a route that imports a
//! module lazily, so the module gets a chunk of its own.

#[allow(dead_code)]
mod start_app;

use std::path::Path;
use std::time::{Duration, Instant};

use start_app::{app, Server};

const LAZY_NOTE: &str = "src/lib/lazy-note.ts";

/// The about route with the fixture's lazily imported note.
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

fn note(text: &str) -> String {
    format!("export const note = {text:?};\n")
}

fn write_note(root: &Path, text: &str) {
    std::fs::write(root.join(LAZY_NOTE), note(text)).unwrap();
}

/// The name of the note's chunk, as the served entry imports it.
fn note_chunk(entry: &str) -> Option<String> {
    let at = entry.find("lazy-note-")?;
    let rest = &entry[at..];
    let end = rest.find(".js")? + ".js".len();
    Some(rest[..end].to_string())
}

fn entry(server: &Server) -> String {
    let (status, body) = server.request("GET", "/@oj-start/client-entry.js", &[]);
    assert_eq!(status, 200, "the client entry\n{}", server.output());
    body
}

/// The note chunk's name once the entry imports one other than `not`.
fn wait_for_note_chunk(server: &Server, not: Option<&str>) -> String {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(name) = note_chunk(&entry(server)) {
            if Some(name.as_str()) != not {
                return name;
            }
        }
        assert!(
            Instant::now() < deadline,
            "the entry never imported a new note chunk\n{}",
            server.output()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn a_rebundle_keeps_serving_the_chunks_of_the_bundle_before_it() {
    let Some(app) = app("chunk-retention") else {
        return;
    };
    let root = &app.0;
    std::fs::write(root.join("src/routes/about.tsx"), ABOUT).unwrap();
    write_note(root, "lazy-note-one");
    let dev = Server::oj_dev(root);

    let old = wait_for_note_chunk(&dev, None);
    let (status, old_body) = dev.request("GET", &format!("/@oj-start/{old}"), &[]);
    assert!(
        status == 200 && old_body.contains("lazy-note-one"),
        "the note chunk before the edit: {status} {old_body:?}\n{}",
        dev.output()
    );

    // The edit renames the chunk; a document served before it still asks
    // for the old name.
    write_note(root, "lazy-note-two");
    let new = wait_for_note_chunk(&dev, Some(&old));
    assert_ne!(new, old, "the edit renamed the chunk");
    let (status, body) = dev.request("GET", &format!("/@oj-start/{new}"), &[]);
    assert!(
        status == 200 && body.contains("lazy-note-two"),
        "the new note chunk: {status} {body:?}\n{}",
        dev.output()
    );
    let (status, body) = dev.request("GET", &format!("/@oj-start/{old}"), &[]);
    assert_eq!(
        status,
        200,
        "the old note chunk after the rebundle: {body:?}\n{}",
        dev.output()
    );
    assert_eq!(body, old_body, "the old chunk serves the code it had");

    // A name no bundle ever had is still not found.
    let (status, _) = dev.request("GET", "/@oj-start/lazy-note-AAAAAAAA.js", &[]);
    assert_eq!(status, 404, "an unknown chunk\n{}", dev.output());
}
