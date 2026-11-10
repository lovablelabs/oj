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
use std::sync::{Arc, Mutex};
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

/// The fixture with the lazily imported note, its dev server, and the note
/// chunk's first name and content.
fn started(name: &str) -> Option<(start_app::App, Server, String, String)> {
    let app = app(name)?;
    std::fs::write(app.0.join("src/routes/about.tsx"), ABOUT).unwrap();
    write_note(&app.0, "lazy-note-one");
    let dev = Server::oj_dev(&app.0);
    let first = wait_for_note_chunk(&dev, None);
    let (status, body) = dev.request("GET", &format!("/@oj-start/{first}"), &[]);
    assert!(
        status == 200 && body.contains("lazy-note-one"),
        "the note chunk before any edit: {status} {body:?}\n{}",
        dev.output()
    );
    Some((app, dev, first, body))
}

fn chunk(dev: &Server, name: &str) -> (u16, String) {
    dev.request("GET", &format!("/@oj-start/{name}"), &[])
}

/// The text frames a page's socket (`/@oj-start/hmr`) receives, as they
/// arrive: an open page of the dev server, as far as it can tell.
struct PageSocket {
    frames: Arc<Mutex<Vec<String>>>,
}

impl PageSocket {
    fn open(dev: &Server) -> PageSocket {
        let frames = Arc::new(Mutex::new(Vec::new()));
        let url = format!("ws://127.0.0.1:{}/@oj-start/hmr", dev.port);
        let (opened, is_open) = std::sync::mpsc::channel();
        let sink = Arc::clone(&frames);
        std::thread::spawn(move || {
            use futures_util::StreamExt;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
                let _ = opened.send(());
                while let Some(Ok(message)) = socket.next().await {
                    if let tokio_tungstenite::tungstenite::Message::Text(text) = message {
                        sink.lock().unwrap().push(text.to_string());
                    }
                }
            });
        });
        is_open
            .recv_timeout(Duration::from_secs(30))
            .expect("the page socket opens");
        PageSocket { frames }
    }

    /// Whether a frame says the chunk `name` is gone, within a few seconds.
    fn told_gone(&self, name: &str) -> bool {
        let wanted = serde_json::json!({ "type": "oj:chunk-gone", "name": name });
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let told = self.frames.lock().unwrap().iter().any(|frame| {
                serde_json::from_str::<serde_json::Value>(frame).is_ok_and(|v| v == wanted)
            });
            if told || Instant::now() > deadline {
                return told;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn gone_frames(&self) -> usize {
        self.frames
            .lock()
            .unwrap()
            .iter()
            .filter(|frame| frame.contains("oj:chunk-gone"))
            .count()
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

// A rebundle that fails (the edit does not parse) serves nothing new, and
// takes nothing away: the chunks the open pages run on, the current bundle's
// and the one before it, keep answering, and the fixed edit's bundle joins
// them. Vite keeps serving what it has through a failed transform the same
// way: the error goes to the overlay, not to the modules already loaded.
#[test]
fn a_failed_rebundle_keeps_every_served_chunk() {
    let Some((app, dev, first, first_body)) = started("chunk-retention-failed") else {
        return;
    };
    let root = &app.0;
    write_note(root, "lazy-note-two");
    let second = wait_for_note_chunk(&dev, Some(&first));
    let (_, second_body) = chunk(&dev, &second);

    let page = PageSocket::open(&dev);
    let from = dev.output().len();
    std::fs::write(root.join(LAZY_NOTE), "export const note = ;\n").unwrap();
    // The failure reaches the page as Vite's error frame.
    let deadline = Instant::now() + Duration::from_secs(60);
    while !page
        .frames
        .lock()
        .unwrap()
        .iter()
        .any(|f| f.contains("\"type\":\"error\""))
    {
        assert!(
            Instant::now() < deadline,
            "the broken edit never failed the bundle\n{}",
            &dev.output()[from..]
        );
        // A document asks for the output: the rebundle runs now.
        let _ = dev.request("GET", "/@oj-start/client-entry.js", &[]);
        std::thread::sleep(Duration::from_millis(200));
    }
    for (name, body) in [(&first, &first_body), (&second, &second_body)] {
        let (status, served) = chunk(&dev, name);
        assert_eq!(
            status,
            200,
            "{name} after the failed rebundle\n{}",
            dev.output()
        );
        assert_eq!(&served, body, "{name} serves the code it had");
    }

    write_note(root, "lazy-note-three");
    let third = wait_for_note_chunk(&dev, Some(&second));
    assert!(chunk(&dev, &third).1.contains("lazy-note-three"));
    for name in [&first, &second] {
        assert_eq!(chunk(&dev, name).0, 200, "{name} after the fixed rebundle");
    }
    assert_eq!(page.gone_frames(), 0, "no chunk was reported gone");
}

// The window is the bundle store's: the current bundle and seven behind it.
// A name older than that is not found, and the server says so to the open
// pages, naming the chunk, so the page that asked reloads (its router caught
// the failed import, so nothing else would tell it). A name still inside the
// window answers, and asking for it tells the pages nothing.
#[test]
fn a_chunk_past_the_window_is_not_found_and_the_pages_are_told() {
    let Some((app, dev, first, _)) = started("chunk-retention-window") else {
        return;
    };
    let root = &app.0;
    let page = PageSocket::open(&dev);
    let mut names = vec![first];
    // Eight rebundles: the first bundle is the ninth from the current one.
    for n in 2..=9 {
        write_note(root, &format!("lazy-note-{n}"));
        let name = wait_for_note_chunk(&dev, names.last().map(String::as_str));
        names.push(name);
    }
    let unique: std::collections::HashSet<&String> = names.iter().collect();
    assert_eq!(unique.len(), names.len(), "every edit renamed the chunk");

    // The second bundle is the oldest still served; the newest is current.
    for name in [&names[1], &names[8]] {
        assert_eq!(chunk(&dev, name).0, 200, "{name}\n{}", dev.output());
    }
    assert_eq!(page.gone_frames(), 0, "a served chunk is not reported gone");

    let (status, body) = chunk(&dev, &names[0]);
    assert_eq!(status, 404, "the first bundle's chunk: {body}");
    assert!(
        page.told_gone(&names[0]),
        "the pages were not told {} is gone: {:?}",
        names[0],
        page.frames.lock().unwrap()
    );
    // A source map is the browser's tools asking, not a page.
    let before = page.gone_frames();
    assert_eq!(chunk(&dev, &format!("{}.map", names[0])).0, 404);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        page.gone_frames(),
        before,
        "a missing source map is no page's chunk"
    );
}

/// Removes every file under `dir` whose content is `content`.
fn remove_files_with(dir: &Path, content: &[u8]) -> usize {
    let mut removed = 0;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            removed += remove_files_with(&path, content);
        } else if kind.is_file()
            && std::fs::metadata(&path).is_ok_and(|m| m.len() == content.len() as u64)
            && std::fs::read(&path).is_ok_and(|bytes| bytes == content)
        {
            std::fs::remove_file(&path).unwrap();
            removed += 1;
        }
    }
    removed
}

// A retained chunk whose file is gone from the cache (the store swept it):
// the page is too old to serve, which is a 404 that says so and a frame to
// the pages, not a server fault.
#[test]
fn a_retained_chunk_whose_file_was_swept_is_not_found_and_the_pages_are_told() {
    let Some((app, dev, first, first_body)) = started("chunk-retention-swept") else {
        return;
    };
    let root = &app.0;
    write_note(root, "lazy-note-two");
    let second = wait_for_note_chunk(&dev, Some(&first));
    assert_eq!(chunk(&dev, &first).0, 200, "retained before the sweep");

    let page = PageSocket::open(&dev);
    let removed = remove_files_with(&root.join(".oj-cache"), first_body.as_bytes());
    assert!(removed > 0, "the first chunk's file was not in the cache");
    let (status, body) = chunk(&dev, &first);
    assert_eq!(status, 404, "{body}\n{}", dev.output());
    assert!(body.contains("no longer on disk"), "{body}");
    assert!(page.told_gone(&first), "{:?}", page.frames.lock().unwrap());
    // The current bundle's chunk is untouched.
    assert_eq!(chunk(&dev, &second).0, 200);
}
