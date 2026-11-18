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
//! module lazily, so the module gets a chunk of its own. The browser test
//! skips without Chrome, unless `OJ_REQUIRE_BROWSER` is set.

#[path = "../../oj_server/tests/dev/browser.rs"]
#[allow(dead_code)]
mod browser;
#[allow(dead_code)]
mod start_app;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use browser::Page;
use start_app::{app, Server, WAIT};

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

/// The home route with a link to the about route, in place of the fixture's
/// (whose page exercises every plugin of the fixture; this one is for the
/// navigation alone), and the hydration probe the browser tests wait for.
const INDEX: &str = r#"import { createRoute, Link } from "@tanstack/react-router";
import { useEffect, useState } from "react";

import { rootRoute } from "./__root";

export const indexRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/",
  component: Index,
});

function Index() {
  const [mounted, setMounted] = useState(false);
  useEffect(() => setMounted(true), []);
  return (
    <main>
      <h1 className="fixture-heading">home</h1>
      {mounted ? <span data-testid="client-mounted">client-mounted-ok</span> : null}
      <Link to="/about" id="to-about">about</Link>
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

// A page is behind by as many bundles as were made since it loaded, which
// under a held reload or a hot session is many: the chunks of a bundle eight
// rebundles back are still served (the dev server kept seven bundles behind
// the current one, the bundle store's window, and the eighth was a 404). A
// name no bundle of this run ever had is not found, and the server says so
// to the open pages, naming the chunk, so the page that asked reloads (its
// router caught the failed import, so nothing else would tell it); a name
// that is served tells the pages nothing.
#[test]
fn a_chunk_of_the_bundle_eight_rebundles_back_is_still_served() {
    let Some((app, dev, first, first_body)) = started("chunk-retention-window") else {
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

    for name in &names {
        let (status, body) = chunk(&dev, name);
        assert_eq!(
            status,
            200,
            "{name} after eight rebundles: {body}\n{}",
            dev.output()
        );
    }
    assert_eq!(
        chunk(&dev, &names[0]).1,
        first_body,
        "the first bundle's chunk serves the code it had"
    );
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(page.gone_frames(), 0, "a served chunk is not reported gone");

    // A name this run of the server never had is told: a page left open
    // across a restart of the dev server names such chunks, and nothing else
    // reloads it (its stamp is another run's).
    let never = "lazy-note-AAAAAAAA.js";
    assert_eq!(chunk(&dev, never).0, 404);
    assert!(
        page.told_gone(never),
        "the pages were not told {never} is gone: {:?}",
        page.frames.lock().unwrap()
    );
    // A source map is the browser's tools asking, not a page.
    let before = page.gone_frames();
    assert_eq!(chunk(&dev, &format!("{never}.map")).0, 404);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        page.gone_frames(),
        before,
        "a missing source map is no page's chunk"
    );
}

/// A page whose reload the editor's gate holds (`OJ_HMR_GATE=1` with full
/// reloads: every edit rebundles, the reload waits for the flush) stays on
/// the entry it loaded while nine edits rebundle behind it. Its first
/// navigation to the about route then imports the note's chunk by the name
/// its entry has, the first bundle's, and the page renders it, where a 404
/// left the page blank until a manual reload; a chunk of the second bundle,
/// never loaded, imports too. The flush then reloads the page onto the
/// current bundle.
#[tokio::test(flavor = "multi_thread")]
async fn a_page_held_behind_many_rebundles_loads_its_lazy_route() {
    let Some(app) = app("chunk-retention-held") else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    std::fs::write(root.join("src/routes/about.tsx"), ABOUT).unwrap();
    std::fs::write(root.join("src/routes/index.tsx"), INDEX).unwrap();
    write_note(&root, "lazy-note-one");
    let dev = Server::oj_dev_with(
        &root,
        &[("OJ_HMR_GATE", "1"), ("OJ_HMR_FULL_RELOAD", "true")],
    );
    let first = wait_for_note_chunk(&dev, None);

    let page = Page::open(&dev.url("/")).await;
    page.wait_for(
        "document.querySelector('[data-testid=\"client-mounted\"]')?.textContent.trim() === 'client-mounted-ok'",
        WAIT,
    )
    .await;
    page.eval::<bool>("(window.__mark = 'kept', true)").await;

    // Nine edits, each renaming the note's chunk; the gate holds the reload.
    let mut names = vec![first];
    for n in 2..=10 {
        write_note(&root, &format!("lazy-note-{n}"));
        let name = wait_for_note_chunk(&dev, names.last().map(String::as_str));
        names.push(name);
    }
    // No hot updates under a full-reload gate: nothing is ever left
    // behind, and a chunk's name is fetched once (every rebundle reloads
    // the pages onto new names).
    let (status, headers, _) = dev.fetch("GET", &format!("/@oj-start/{}", names[0]), &[]);
    assert_eq!(status, 200, "the first bundle's chunk\n{}", dev.output());
    assert_eq!(
        headers
            .iter()
            .find(|(name, _)| name == "cache-control")
            .map(|(_, value)| value.as_str()),
        Some("max-age=31536000, immutable")
    );
    assert!(
        dev.output().contains("reload held for the editor's flush"),
        "the gate held no reload\n{}",
        dev.output()
    );
    assert!(
        page.eval::<bool>("window.__mark === 'kept'").await,
        "the page reloaded while the gate held its reload"
    );

    // The page's own lazy route: its entry imports the first bundle's chunk.
    page.eval::<bool>("(document.getElementById('to-about').click(), true)")
        .await;
    let note = "document.querySelector('#note')?.textContent";
    page.wait_for(
        &format!("({note} === 'lazy-note-one') || window.__mark !== 'kept'"),
        WAIT,
    )
    .await;
    assert!(
        page.eval::<bool>("window.__mark === 'kept'").await,
        "the page reloaded for a chunk the dev server no longer served\n{}",
        dev.output()
    );
    assert_eq!(
        page.eval::<String>(note).await,
        "lazy-note-one",
        "the about route rendered the note of the bundle the page loaded"
    );

    // A chunk of the second bundle, which nothing on the page loaded yet.
    let second = format!("/@oj-start/{}", names[1]);
    page.eval::<bool>(&format!(
        "(import({}).then(m => {{ window.__second = m.note }}, e => {{ window.__second = 'failed: ' + e.message }}), true)",
        serde_json::to_string(&second).unwrap()
    ))
    .await;
    page.wait_for(
        "typeof window.__second === 'string' || window.__mark !== 'kept'",
        WAIT,
    )
    .await;
    assert_eq!(
        page.eval::<serde_json::Value>("window.__second").await,
        serde_json::json!("lazy-note-2"),
        "the second bundle's chunk\n{}",
        dev.output()
    );

    // The flush releases the reload: the page comes back on the current
    // bundle, at the about route, with the current note.
    let (status, body) = dev.request("POST", "/__hmr_flush", &[]);
    assert_eq!(status, 200, "flush: {body}");
    page.wait_for(&format!("{note} === 'lazy-note-10'"), WAIT)
        .await;
    assert!(
        page.eval::<bool>("window.__mark === undefined").await,
        "the flush reloaded the page"
    );
    page.close().await;
}

// ---- Hot updates: a chunk whose code was patched since is not served ----
//
// With hot updates on, a page takes an edit in place and keeps the entry it
// loaded, which imports each lazy chunk by the name it had then. A chunk the
// page already ran is patched; one it has not loaded yet is not the hot
// runtime's to load: the entry imports it natively, by the old name, and the
// retained chunk under that name is the code from before the edit. The dev
// server answers that such a chunk is gone, and the page that asked reloads
// onto the current output.

const NOTE_SHOWN: &str = "document.querySelector('#note')?.textContent";
const MOUNTED: &str = "document.querySelector('[data-testid=\"client-mounted\"]')?.textContent.trim() === 'client-mounted-ok'";
/// Whether the page's hot client is up: the bundle's runtime is there and
/// its socket is open (the introduction is sent again, which the server
/// takes once). The server registers a client as it reads the introduction,
/// well inside the 50 ms the watcher waits before it looks at a save.
const HOT_CLIENT: &str = "(() => { const bus = globalThis.__ojStartHmr; return !!(bus && bus.runtime && bus.send && bus.send({ type: 'oj:client', id: bus.runtime.clientId })); })()";

/// Waits until `page` shows a new document (no mark on it) that hydrated
/// (`ready`) with its hot client up, and marks the document: a reload
/// takes the mark away.
async fn hot_ready(page: &Page, ready: &str) {
    page.wait_for(
        &format!("window.__mark === undefined && ({ready}) && {HOT_CLIENT}"),
        WAIT,
    )
    .await;
    page.eval::<bool>("(window.__mark = 'kept', true)").await;
}

async fn kept(page: &Page) -> bool {
    page.eval::<bool>("window.__mark === 'kept'").await
}

/// Loads `path` in the page's tab as a new document (the browser's HTTP
/// cache and session storage stay), and waits for it as [`hot_ready`] does.
async fn load(page: &Page, path: &str, ready: &str) {
    page.eval::<bool>(&format!(
        "(window.__mark = 'leaving', location.assign({}), true)",
        serde_json::to_string(path).unwrap()
    ))
    .await;
    hot_ready(page, ready).await;
}

fn hot_updates(dev: &Server) -> usize {
    dev.output().matches("oj start: hot updated").count()
}

/// Waits for a "hot updated" line past `before` of them: the edit went to
/// the pages in place, with no reload.
async fn hot_updated(dev: &Server, before: usize) {
    let deadline = Instant::now() + WAIT;
    while hot_updates(dev) <= before {
        assert!(
            Instant::now() < deadline,
            "the edit was not a hot update\n{}",
            dev.output()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Writes the note and waits for its hot update.
async fn edit_note(dev: &Server, root: &Path, text: &str) {
    let before = hot_updates(dev);
    write_note(root, text);
    hot_updated(dev, before).await;
}

/// Follows the page's link to the about route; whether it comes to show
/// `text` as the note.
async fn goes_to_about_and_shows(page: &Page, text: &str) -> bool {
    page.eval::<bool>("(document.getElementById('to-about').click(), true)")
        .await;
    page.poll(
        &format!("{NOTE_SHOWN} === '{text}'"),
        Duration::from_secs(40),
    )
    .await
}

/// What the page shows and said, for a failed assertion.
async fn seen(page: &Page, dev: &Server) -> String {
    format!(
        "note {:?}; console {:?}; errors {:?}\n{}",
        page.eval::<serde_json::Value>(NOTE_SHOWN).await,
        page.console(),
        page.errors(),
        dev.output()
    )
}

/// The fixture with the home and about routes of this file and the lazily
/// imported note, under `oj dev` with `env`, and the note chunk's first name.
fn hot_started(name: &str, env: &[(&str, &str)]) -> Option<(start_app::App, Server, String)> {
    let app = app(name)?;
    std::fs::write(app.0.join("src/routes/about.tsx"), ABOUT).unwrap();
    std::fs::write(app.0.join("src/routes/index.tsx"), INDEX).unwrap();
    write_note(&app.0, "lazy-note-one");
    let dev = Server::oj_dev_with(&app.0, env);
    let first = wait_for_note_chunk(&dev, None);
    Some((app, dev, first))
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

/// A page open on the home route has not loaded the note's chunk. The note
/// is edited: the page runs no module of it, so the hot update re-runs
/// nothing and reloads nothing. The page then goes to the about route,
/// whose effect imports the note's chunk by the name the page's entry has,
/// the one from before the edit. It used to render the note as it was
/// before the edit for as long as it stayed open, while a new page rendered
/// the edited one. The chunk is now answered as gone (never cached: an edit
/// taken back makes the name current again), the page reloads, and the
/// about route shows the edited note.
#[tokio::test(flavor = "multi_thread")]
async fn an_open_page_first_imports_a_hot_updated_chunk_as_it_is_now() {
    let Some((app, dev, first)) = hot_started("chunk-retention-hot", &[]) else {
        return;
    };
    if browser::skip() {
        return;
    }
    // With hot updates on a chunk is revalidated, not cached for good: a
    // name can stop being served.
    let (status, headers, _) = dev.fetch("GET", &format!("/@oj-start/{first}"), &[]);
    assert_eq!(status, 200);
    assert_eq!(header(&headers, "cache-control"), Some("no-cache"));
    let etag = header(&headers, "etag").expect("a validator").to_string();
    let (status, _, _) = dev.fetch(
        "GET",
        &format!("/@oj-start/{first}"),
        &[("If-None-Match", etag)],
    );
    assert_eq!(status, 304, "the chunk revalidates");

    let page = Page::open(&dev.url("/")).await;
    hot_ready(&page, MOUNTED).await;
    edit_note(&dev, &app.0, "lazy-note-two").await;
    assert!(
        kept(&page).await,
        "an edit to a module the page does not run reloaded it\n{}",
        dev.output()
    );

    assert!(
        goes_to_about_and_shows(&page, "lazy-note-two").await,
        "the about route of a page open since before the edit: {}",
        seen(&page, &dev).await
    );
    assert!(
        !kept(&page).await,
        "the page shows the edited note without having reloaded"
    );

    // The chunk from before the edit is no longer served, and the answer is
    // not one to cache; the current one is served.
    let (status, headers, body) = dev.fetch("GET", &format!("/@oj-start/{first}"), &[]);
    let body = String::from_utf8_lossy(&body);
    assert_eq!(status, 404, "{body}\n{}", dev.output());
    assert!(body.contains("hot-updated since"), "{body}");
    assert_eq!(header(&headers, "cache-control"), Some("no-store"));
    let second = wait_for_note_chunk(&dev, Some(&first));
    assert_eq!(chunk(&dev, &second).0, 200);
    page.close().await;
}

/// The same with the chunk in the browser's cache: the tab visited the about
/// route before (the note's chunk was fetched then), loaded the home route
/// as a new document, and the note was edited. Served as immutable, the
/// chunk came out of the browser's cache when the page went to the about
/// route, the dev server was never asked, and the page ran the note from
/// before the edit. With hot updates on a chunk is revalidated, so the
/// browser asks, hears it is gone, and the page reloads.
#[tokio::test(flavor = "multi_thread")]
async fn a_hot_updated_chunk_is_not_run_from_the_browsers_cache() {
    let Some((app, dev, _first)) = hot_started("chunk-retention-cached", &[]) else {
        return;
    };
    if browser::skip() {
        return;
    }
    let page = Page::open(&dev.url("/about")).await;
    page.wait_for(&format!("{NOTE_SHOWN} === 'lazy-note-one'"), WAIT)
        .await;
    load(&page, "/", MOUNTED).await;
    edit_note(&dev, &app.0, "lazy-note-two").await;
    assert!(kept(&page).await, "the edit reloaded the page");

    assert!(
        goes_to_about_and_shows(&page, "lazy-note-two").await,
        "the about route, its chunk in the browser's cache: {}",
        seen(&page, &dev).await
    );
    assert!(!kept(&page).await, "the page reloaded for the chunk");
    page.close().await;
}

/// The editor's gate holds hot updates until its flush (`OJ_HMR_GATE=1`
/// without full reloads). A page that imports the edited chunk during the
/// hold has not been patched: it is whole with the chunks it names, so it
/// gets the chunk as it was and is not reloaded (a reload would show it
/// every held edit, ahead of the flush). The flush releases the patch,
/// which re-runs the module the page has run by then. A page that first
/// imports the chunk after the flush is past the patch, and reloads.
#[tokio::test(flavor = "multi_thread")]
async fn a_held_hot_update_leaves_its_chunk_served_until_the_flush() {
    let gate = [("OJ_HMR_GATE", "1"), ("OJ_HMR_FULL_RELOAD", "false")];
    let Some((app, dev, first)) = hot_started("chunk-retention-gate", &gate) else {
        return;
    };
    if browser::skip() {
        return;
    }
    let during = Page::open(&dev.url("/")).await;
    let after = Page::open(&dev.url("/")).await;
    for page in [&during, &after] {
        hot_ready(page, MOUNTED).await;
    }

    // The gate holds from the first save it sees until the flush. The
    // engine watches the files on its own clock and can patch the pages
    // before the gate has seen that save (such a patch goes straight out,
    // and its chunk is left behind at once, which is the same thing said
    // twice); so the gate is made to hold first, by a save of a file no
    // bundle has, as an editor's batch of writes holds it for the later
    // ones.
    let held = |file: &'static str| {
        let dev = &dev;
        async move {
            let deadline = Instant::now() + WAIT;
            loop {
                let (_, status) = dev.request("GET", "/__hmr_gate", &[]);
                if status.contains(&format!("{file}\":")) {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "the gate never held {file}\n{}",
                    dev.output()
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    };
    std::fs::write(app.0.join("src/lib/unused.ts"), "export {};\n").unwrap();
    held("/src/lib/unused.ts").await;
    let before = hot_updates(&dev);
    write_note(&app.0, "lazy-note-two");
    held("/src/lib/lazy-note.ts").await;
    hot_updated(&dev, before).await;
    // The output behind the held update renamed the chunk.
    let second = wait_for_note_chunk(&dev, Some(&first));

    // During the hold: the chunk as the page's entry names it, no reload.
    assert!(
        goes_to_about_and_shows(&during, "lazy-note-one").await,
        "a page importing the chunk while the gate holds its update: {}",
        seen(&during, &dev).await
    );
    assert!(
        kept(&during).await,
        "the page reloaded onto edits the gate still holds\n{}",
        dev.output()
    );
    assert_eq!(chunk(&dev, &first).0, 200, "served while the gate holds");

    let (status, body) = dev.request("POST", "/__hmr_flush", &[]);
    assert_eq!(status, 200, "flush: {body}");
    // The released patch re-runs the note on the page that ran it.
    assert!(
        during
            .poll(
                &format!("{NOTE_SHOWN} === 'lazy-note-two'"),
                Duration::from_secs(40)
            )
            .await,
        "the page after the flush: {}",
        seen(&during, &dev).await
    );
    // After the flush the old name is left behind: the other page reloads.
    assert_eq!(chunk(&dev, &first).0, 404, "left behind once released");
    assert_eq!(chunk(&dev, &second).0, 200);
    assert!(
        goes_to_about_and_shows(&after, "lazy-note-two").await,
        "a page importing the chunk after the flush: {}",
        seen(&after, &dev).await
    );
    assert!(!kept(&after).await, "the page reloaded for the chunk");
    during.close().await;
    after.close().await;
}

/// An edit made, taken back and made again renames the chunk back and
/// forth. A tab that loads the home route anew each time and then goes to
/// the about route reloads for the chunk each time: the third time it meets
/// the name it reloaded for the first time, under a later document, and
/// used to be left with a failed import and no note (the reload guard is
/// once per document and chunk, and took the two documents for one).
#[tokio::test(flavor = "multi_thread")]
async fn an_edit_taken_back_and_made_again_reloads_the_page_each_time() {
    let Some((app, dev, first)) = hot_started("chunk-retention-toggle", &[]) else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = &app.0;
    let tab = Page::open(&dev.url("/")).await;
    hot_ready(&tab, MOUNTED).await;

    let mut current = first.clone();
    for (round, text) in ["lazy-note-two", "lazy-note-one", "lazy-note-two"]
        .into_iter()
        .enumerate()
    {
        edit_note(&dev, root, text).await;
        assert!(kept(&tab).await, "round {round}: the edit reloaded the tab");
        assert!(
            goes_to_about_and_shows(&tab, text).await,
            "round {round}: the about route after the edit to {text:?}: {}",
            seen(&tab, &dev).await
        );
        assert!(
            !kept(&tab).await,
            "round {round}: the tab reloaded for the chunk"
        );
        let renamed = wait_for_note_chunk(&dev, Some(&current));
        assert_eq!(chunk(&dev, &current).0, 404, "round {round}: left behind");
        current = renamed;
        load(&tab, "/", MOUNTED).await;
    }
    // Each reload was the page's own, for its chunk: the server sent none.
    assert!(
        !dev.output().contains("reloading"),
        "the server reloaded the pages\n{}",
        dev.output()
    );
    assert!(
        tab.console()
            .iter()
            .all(|line| !line.contains("served the same document")
                && !line.contains("gone from the dev server")),
        "the tab's console: {:?}",
        tab.console()
    );
    tab.close().await;
}

/// An edit taken back makes the chunk's first name the current one again:
/// it is served, and a page open since before the edit, which never loaded
/// the chunk, imports it by that name and shows the note as it is, without
/// a reload.
#[tokio::test(flavor = "multi_thread")]
async fn an_edit_taken_back_serves_the_chunk_under_its_first_name_again() {
    let Some((app, dev, first)) = hot_started("chunk-retention-revert", &[]) else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = &app.0;
    let page = Page::open(&dev.url("/")).await;
    hot_ready(&page, MOUNTED).await;

    edit_note(&dev, root, "lazy-note-two").await;
    let second = wait_for_note_chunk(&dev, Some(&first));
    assert_eq!(chunk(&dev, &first).0, 404, "left behind by the edit");

    edit_note(&dev, root, "lazy-note-one").await;
    let third = wait_for_note_chunk(&dev, Some(&second));
    assert_eq!(third, first, "the note as it was has its first name");
    let (status, body) = chunk(&dev, &first);
    assert!(
        status == 200 && body.contains("lazy-note-one"),
        "the first name after the edit was taken back: {status} {body:?}\n{}",
        dev.output()
    );
    assert_eq!(chunk(&dev, &second).0, 404, "the edit's own chunk");

    assert!(kept(&page).await, "the edits reloaded the page");
    assert!(
        goes_to_about_and_shows(&page, "lazy-note-one").await,
        "a page open since before the edit: {}",
        seen(&page, &dev).await
    );
    assert!(kept(&page).await, "it had no reason to reload");
    page.close().await;
}

/// A hot client of the dev server without a browser: a socket that
/// introduces itself as a page's runtime does, so edits go out as hot
/// updates.
fn hot_socket(dev: &Server) {
    let url = format!("ws://127.0.0.1:{}/@oj-start/hmr", dev.port);
    let (opened, is_open) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use futures_util::{SinkExt, StreamExt};
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
            socket
                .send(tokio_tungstenite::tungstenite::Message::Text(
                    r#"{"type":"oj:client","id":"socket-page"}"#.into(),
                ))
                .await
                .unwrap();
            let _ = opened.send(());
            while let Some(Ok(_)) = socket.next().await {}
        });
    });
    is_open
        .recv_timeout(Duration::from_secs(30))
        .expect("the hot socket opens");
    // The server reads the introduction and registers the client.
    std::thread::sleep(Duration::from_millis(500));
}

// The engine watches the files itself and patches the pages on its own
// clock; the dev server's watcher looks at a save some 50 ms after it. A
// document asked for in between makes the output current at once, renaming
// the chunk before the watcher's run has looked at the save. The chunk is
// left behind whichever comes first: what counts is the engine's own
// account of what it patched and what it then put out. (Which of the two
// comes first here is the machine's to decide; the bookkeeping's unit tests
// in start_hot.rs pin the order.)
#[test]
fn a_chunk_is_left_behind_when_a_document_asks_for_the_output_first() {
    let Some((app, dev, first)) = hot_started("chunk-retention-race", &[]) else {
        return;
    };
    hot_socket(&dev);
    let before = hot_updates(&dev);
    write_note(&app.0, "lazy-note-two");
    // Documents, back to back, from the save until the watcher's run ends.
    let deadline = Instant::now() + WAIT;
    while hot_updates(&dev) <= before {
        let (status, _) = dev.request("GET", "/", &[]);
        assert_eq!(status, 200, "{}", dev.output());
        assert!(
            Instant::now() < deadline,
            "the edit was not a hot update\n{}",
            dev.output()
        );
    }
    let second = wait_for_note_chunk(&dev, Some(&first));
    assert_eq!(
        chunk(&dev, &first).0,
        404,
        "the chunk from before the edit is still served\n{}",
        dev.output()
    );
    assert_eq!(chunk(&dev, &second).0, 200);
}

// ---- A file-routed app: the lazy chunk is a route's split-off component ----

const FILE_ROOT: &str = r#"import { createRootRoute, HeadContent, Outlet, Scripts } from "@tanstack/react-router";
import { useEffect } from "react";

export const Route = createRootRoute({
  head: () => ({ meta: [{ title: "lazy routes" }] }),
  component: RootComponent,
});

function RootComponent() {
  useEffect(() => {
    (self as { __ojHydrated?: boolean }).__ojHydrated = true;
  }, []);
  return (
    <html lang="en">
      <head>
        <HeadContent />
      </head>
      <body>
        <Outlet />
        <Scripts />
      </body>
    </html>
  );
}
"#;

const FILE_INDEX: &str = r#"import { createFileRoute, Link } from "@tanstack/react-router";

export const Route = createFileRoute("/")({
  component: () => (
    <main>
      <p id="home">home page</p>
      <Link to="/later" id="to-later">later</Link>
    </main>
  ),
});
"#;

const FILE_LAZY: &str = r#"import { createFileRoute } from "@tanstack/react-router";
import { lazyLabel } from "../lib/lazy-label";

export const Route = createFileRoute("/later")({
  component: LazyPage,
});

function LazyPage() {
  return <p id="later">{lazyLabel}</p>;
}
"#;

const FILE_ROUTER: &str = r#"import { createRouter } from "@tanstack/react-router";
import { routeTree } from "./routeTree.gen";

export function getRouter() {
  return createRouter({ routeTree });
}
"#;

/// The chunk files under `dir` whose text holds `needle`, by name.
fn chunks_holding(dir: &Path, needle: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "js"))
        .filter(|e| std::fs::read_to_string(e.path()).is_ok_and(|s| s.contains(needle)))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

/// A file-routed app: the router's code splitter moves a route's component
/// into `route.tsx?tsr-split=component`, a chunk of its own with what the
/// component imports. A page open on the home route has not loaded the lazy
/// route's chunk; a module that chunk holds is edited (a hot update, no
/// reload), and the page follows its link to the route: it shows the route
/// as it is now, after a reload, where it showed it as it was before the
/// edit.
#[tokio::test(flavor = "multi_thread")]
async fn an_open_page_goes_to_a_lazy_route_hot_updated_since_as_it_is_now() {
    let Some(app) = app("chunk-retention-file-routes") else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    let routes = root.join("src/routes");
    std::fs::remove_dir_all(&routes).unwrap();
    std::fs::create_dir_all(&routes).unwrap();
    std::fs::write(routes.join("__root.tsx"), FILE_ROOT).unwrap();
    std::fs::write(routes.join("index.tsx"), FILE_INDEX).unwrap();
    std::fs::write(routes.join("later.tsx"), FILE_LAZY).unwrap();
    std::fs::write(root.join("src/routeTree.ts"), "export {};\n").unwrap();
    std::fs::write(root.join("src/router.tsx"), FILE_ROUTER).unwrap();
    let label = root.join("src/lib/lazy-label.ts");
    std::fs::write(&label, "export const lazyLabel = \"lazy-label-one\";\n").unwrap();
    let dev = Server::oj_dev(&root);

    let page = Page::open(&dev.url("/")).await;
    hot_ready(&page, "self.__ojHydrated === true").await;
    // The label is in the lazy route's split-off chunk, not in the entry.
    let chunks = root.join(".oj-cache/v1/start/client-chunks");
    let holding = chunks_holding(&chunks, "lazy-label-one");
    assert_eq!(holding.len(), 1, "the label's chunk: {holding:?}");
    let old = holding[0].clone();
    assert_ne!(old, "client-entry.js");
    assert!(
        std::fs::read_to_string(chunks.join(&old))
            .unwrap()
            .contains("later.tsx?tsr-split=component"),
        "{old} is not the route's split-off component"
    );

    let before = hot_updates(&dev);
    std::fs::write(&label, "export const lazyLabel = \"lazy-label-two\";\n").unwrap();
    hot_updated(&dev, before).await;
    assert!(kept(&page).await, "the edit reloaded the page");

    page.eval::<bool>("(document.getElementById('to-later').click(), true)")
        .await;
    let shown = "document.querySelector('#later')?.textContent";
    assert!(
        page.poll(
            &format!("{shown} === 'lazy-label-two'"),
            Duration::from_secs(40)
        )
        .await,
        "the lazy route shows {:?}; console {:?}; errors {:?}\n{}",
        page.eval::<serde_json::Value>(shown).await,
        page.console(),
        page.errors(),
        dev.output()
    );
    assert!(
        !kept(&page).await,
        "the page reloaded for the route's chunk"
    );
    assert_eq!(chunk(&dev, &old).0, 404, "the route's old chunk");
    page.close().await;
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
