// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A module an open page reaches only through `import()` (a lazy route's
//! split-off component, a module imported from a handler), first imported
//! after a hot update changed it, a module it imports, or re-ran a module it
//! imports. The page's entry used to import the module's chunk by the name
//! it was built with, and the dev server served that name with the code
//! from before the update, bound to the modules of that build: the page ran
//! the module as it was before the edit, against the page's first instances
//! of what it imports, until it was reloaded. Under Vite's dev server an
//! `import()` is a request for the module when it runs: the page gets the
//! current code (`transformMiddleware` answers `no-cache`), bound to the
//! current instance of each module it imports (import analysis gives a
//! hot-updated import its `?t=`), with no reload. The page's bundle now asks
//! the dev server for the module by id when the `import()` runs. The browser
//! tests skip without Chrome, unless `OJ_REQUIRE_BROWSER` is set.

#[path = "../../oj_server/tests/dev/browser.rs"]
#[allow(dead_code)]
mod browser;
#[allow(dead_code)]
mod start_app;

use std::path::Path;
use std::time::Duration;

use browser::Page;
use start_app::{app, Server, WAIT};

const GATE: &[(&str, &str)] = &[("OJ_HMR_GATE", "1"), ("OJ_HMR_FULL_RELOAD", "false")];
const EVENT: &str = "src/lib/event.ts";
const ABOUT: &str = "src/routes/about.tsx";
const INDEX: &str = "src/routes/index.tsx";
const SHOWN: &str = "(document.querySelector('#about')?.textContent ?? '')";

const ROOT: &str = r#"import { createRootRoute, HeadContent, Link, Outlet, Scripts } from "@tanstack/react-router";
import { useEffect, useState } from "react";

export const Route = createRootRoute({
  head: () => ({ meta: [{ title: "lazy imports" }] }),
  component: RootComponent,
});

function RootComponent() {
  const [clicks, setClicks] = useState(0);
  const [note, setNote] = useState("none");
  useEffect(() => {
    (self as { __ojHydrated?: boolean }).__ojHydrated = true;
  }, []);
  return (
    <html lang="en">
      <head>
        <HeadContent />
      </head>
      <body>
        <Link to="/about" id="to-about">about</Link>
        <button id="count" onClick={() => setClicks(clicks + 1)}>{clicks}</button>
        <button
          id="load"
          onClick={() => import("../lib/note").then((m) => setNote(m.note), (e) => setNote("failed: " + e.message))}
        >
          load
        </button>
        <span id="note">{note}</span>
        <Outlet />
        <Scripts />
      </body>
    </html>
  );
}
"#;

const NOTE: &str = "src/lib/note.ts";
const NOTE_SHOWN: &str = "(document.querySelector('#note')?.textContent ?? '')";

const ROUTER: &str = r#"import { createRouter } from "@tanstack/react-router";
import { routeTree } from "./routeTree.gen";

export function getRouter() {
  return createRouter({ routeTree });
}
"#;

/// The router preloading a route when its link is hovered or focused.
const ROUTER_PRELOAD: &str = r#"import { createRouter } from "@tanstack/react-router";
import { routeTree } from "./routeTree.gen";

export function getRouter() {
  return createRouter({ routeTree, defaultPreload: "intent" });
}
"#;

/// A home route reading the shared module too.
const INDEX_READS: &str = r#"import { createFileRoute } from "@tanstack/react-router";
import { EVENTS } from "../lib/event";

export const Route = createFileRoute("/")({
  component: HomePage,
});

function HomePage() {
  return <p id="home">{EVENTS.join(",")}</p>;
}
"#;

/// The home route; it imports nothing from the shared module.
fn index(text: &str) -> String {
    format!(
        r#"import {{ createFileRoute }} from "@tanstack/react-router";

export const Route = createFileRoute("/")({{
  component: HomePage,
}});

function HomePage() {{
  return <p id="home">{text}</p>;
}}
"#
    )
}

/// The lazy route (its component in a chunk of its own), importing `names`
/// from the shared module and rendering `shown`.
fn about(names: &str, shown: &str) -> String {
    format!(
        r#"import {{ createFileRoute }} from "@tanstack/react-router";
import {{ {names} }} from "../lib/event";

export const Route = createFileRoute("/about")({{
  component: AboutPage,
}});

function AboutPage() {{
  return <p id="about">{{{shown}}}</p>;
}}
"#
    )
}

fn write(root: &Path, rel: &str, source: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, source).unwrap();
}

fn lazy_app(name: &str, event: &str) -> Option<start_app::App> {
    let app = app(name)?;
    let root = &app.0;
    std::fs::remove_dir_all(root.join("src/routes")).unwrap();
    write(root, "src/routes/__root.tsx", ROOT);
    write(root, INDEX, &index("home"));
    write(root, ABOUT, &about("EVENTS", "EVENTS.length"));
    write(root, EVENT, event);
    write(root, NOTE, "export const note = \"note v1\";\n");
    write(root, "src/routeTree.ts", "export {};\n");
    write(root, "src/router.tsx", ROUTER);
    Some(app)
}

/// Writes `files` and waits until the editor's gate holds them all.
async fn held(dev: &Server, root: &Path, files: &[(&str, &str)]) {
    for (rel, source) in files {
        write(root, rel, source);
    }
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let (_, status) = dev.request("GET", "/__hmr_gate", &[]);
        if files
            .iter()
            .all(|(rel, _)| status.contains(&format!("/{rel}\":")))
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the gate never held {files:?}: {status}\n{}",
            dev.output()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn flush(dev: &Server) {
    let (status, body) = dev.request("POST", "/__hmr_flush", &[]);
    assert_eq!(status, 200, "flush: {body}");
}

/// An editor's change of `files`, flushed once the gate holds them all.
async fn change(dev: &Server, root: &Path, files: &[(&str, &str)]) {
    held(dev, root, files).await;
    flush(dev);
}

/// Opens `/` and waits until it hydrated and its hot client registered.
async fn open_home(dev: &Server) -> Page {
    open_at(dev, "/").await
}

async fn open_at(dev: &Server, path: &str) -> Page {
    let page = Page::open(&dev.url(path)).await;
    hot_ready(&page).await;
    page
}

/// The page hydrated and told the dev server which hot client it is, over
/// an open socket: an edit made before that reloads it instead of patching.
const HOT_CLIENT: &str = "(() => { const bus = globalThis.__ojStartHmr; return !!(bus && bus.runtime && bus.send && bus.send({ type: 'oj:client', id: bus.runtime.clientId })); })()";

/// Waits for a new document (no mark on it yet) that hydrated with its hot
/// client up, and marks it: a reload takes the mark away.
async fn hot_ready(page: &Page) {
    page.wait_for(
        &format!("window.__mark === undefined && self.__ojHydrated === true && {HOT_CLIENT}"),
        WAIT,
    )
    .await;
    page.eval::<bool>("(window.__mark = 'kept', true)").await;
}

/// How many changes the dev server has taken in: hot updates and rebuilds.
fn taken_in(dev: &Server) -> usize {
    let out = dev.output();
    out.matches("oj start: hot updated").count() + out.matches("oj start: rebuilt").count()
}

/// Waits until the bundler took in a change made after `before` was read.
async fn took_in(dev: &Server, before: usize) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while taken_in(dev) <= before {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the bundler never took the change in\n{}",
            dev.output()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Imports the note from the layout's button until the page shows `text`:
/// an import made before the bundler took an edit in is of the module from
/// before it.
async fn notes_once_current(page: &Page, dev: &Server, text: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        load_note(page).await;
        if page
            .poll(
                &format!("{NOTE_SHOWN} === {}", serde_json::to_string(text).unwrap()),
                Duration::from_millis(700),
            )
            .await
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the page never showed the note {text:?}: shows {:?}; page errors: {:?}\n{}",
            page.eval::<String>(NOTE_SHOWN).await,
            page.errors(),
            dev.output()
        );
    }
}

/// Waits until the page took in its hot update number `seq` (the runtime
/// counts every one it handles, applied or not).
async fn heard(page: &Page, dev: &Server, seq: u32) {
    let took = page
        .poll(
            &format!("(globalThis.__rolldown_runtime__?.hmr?.lastSeq ?? 0) >= {seq}"),
            WAIT,
        )
        .await;
    assert!(
        took,
        "the page never took hot update {seq}; page errors: {:?}\n{}",
        page.errors(),
        dev.output()
    );
}

/// The page shows `text` on the lazy route with no uncaught error and no
/// render error on the way.
async fn shows(page: &Page, dev: &Server, text: &str) {
    let shown = page
        .poll(
            &format!("{SHOWN} === {}", serde_json::to_string(text).unwrap()),
            Duration::from_secs(40),
        )
        .await;
    assert!(
        shown,
        "the page never showed {text:?}: shows {:?}; page errors: {:?}; console: {:?}\n{}",
        page.eval::<String>(SHOWN).await,
        page.errors(),
        page.console(),
        dev.output()
            .lines()
            .filter(|l| l.contains("oj start:"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert_eq!(
        page.eval::<String>("location.pathname").await,
        "/about",
        "the page left the route it navigated to"
    );
    assert!(page.errors().is_empty(), "page errors: {:?}", page.errors());
    let rendered_errors: Vec<String> = page
        .console()
        .into_iter()
        .filter(|line| line.starts_with("error:") && !line.contains("favicon.ico"))
        .collect();
    assert!(
        rendered_errors.is_empty(),
        "a render threw on the way to {text:?}: {rendered_errors:?}"
    );
}

/// The document is the one the test opened: nothing reloaded the page, and
/// nothing does for a while.
async fn kept(page: &Page, dev: &Server) {
    for _ in 0..2 {
        assert_eq!(
            page.eval::<String>("String(window.__mark)").await,
            "kept",
            "the page was reloaded\n{}",
            dev.output()
                .lines()
                .filter(|l| l.contains("oj start:"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }
}

async fn go_to_about(page: &Page) {
    page.eval::<bool>("(document.querySelector('#to-about').click(), true)")
        .await;
}

/// One edit gives the shared module an export and makes the lazy route read
/// it; the first visit shows the new value with no reload (the layout keeps
/// its state), and the route's next edit is taken in place.
#[tokio::test(flavor = "multi_thread")]
async fn an_unloaded_lazy_route_reads_an_export_added_with_it() {
    let Some(app) = lazy_app(
        "lazy-stale-added",
        "export const EVENTS = [\"a\", \"b\"];\n",
    ) else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    let dev = Server::oj_dev_with(&root, GATE);
    let page = open_home(&dev).await;
    // State a reload would lose.
    page.eval::<bool>("(document.querySelector('#count').click(), true)")
        .await;

    let reads = about(
        "EVENTS, FEATURED, COUNT",
        "EVENTS.length + \"|\" + FEATURED.map((f) => f.toUpperCase()).join(\",\") + \"|\" + COUNT",
    );
    change(
        &dev,
        &root,
        &[
            (
                EVENT,
                "export const EVENTS = [\"a\", \"b\"];\nexport const FEATURED = [\"a\"];\nexport const COUNT = 3;\n",
            ),
            (ABOUT, &reads),
        ],
    )
    .await;
    heard(&page, &dev, 1).await;
    go_to_about(&page).await;
    shows(&page, &dev, "2|A|3").await;
    kept(&page, &dev).await;

    change(
        &dev,
        &root,
        &[(ABOUT, &reads.replace("id=\"about\">", "id=\"about\">v2 "))],
    )
    .await;
    shows(&page, &dev, "v2 2|A|3").await;
    kept(&page, &dev).await;
    assert_eq!(
        page.eval::<String>("document.querySelector('#count').textContent")
            .await,
        "1",
        "the layout lost its state"
    );
    page.close().await;
}

/// One edit drops an export and changes the value of another, which the
/// lazy route starts reading: the first visit and the next edit show the
/// new value, never the old one.
#[tokio::test(flavor = "multi_thread")]
async fn an_unloaded_lazy_route_reads_the_values_of_its_edit() {
    let Some(app) = lazy_app(
        "lazy-stale-removed",
        "export const EVENTS = [\"a\", \"b\"];\nexport const NAMES = [\"a\"];\n",
    ) else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    let dev = Server::oj_dev_with(&root, GATE);
    let page = open_home(&dev).await;

    let reads = about("NAMES", "\"names:\" + NAMES.join(\",\")");
    change(
        &dev,
        &root,
        &[
            (EVENT, "export const NAMES = [\"a\", \"b\"];\n"),
            (ABOUT, &reads),
        ],
    )
    .await;
    heard(&page, &dev, 1).await;
    go_to_about(&page).await;
    shows(&page, &dev, "names:a,b").await;
    kept(&page, &dev).await;

    change(
        &dev,
        &root,
        &[(ABOUT, &reads.replace("id=\"about\">", "id=\"about\">v2 "))],
    )
    .await;
    shows(&page, &dev, "v2 names:a,b").await;
    kept(&page, &dev).await;
    page.close().await;
}

/// A hot update that changed nothing in the lazy route's chunk: the route
/// loads in place, with no reload.
#[tokio::test(flavor = "multi_thread")]
async fn a_lazy_route_no_update_changed_loads_in_place() {
    let Some(app) = lazy_app(
        "lazy-stale-control",
        "export const EVENTS = [\"a\", \"b\"];\n",
    ) else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    let dev = Server::oj_dev_with(&root, GATE);
    let page = open_home(&dev).await;

    change(&dev, &root, &[(INDEX, &index("home v2"))]).await;
    page.wait_for(
        "document.querySelector('#home')?.textContent === 'home v2'",
        WAIT,
    )
    .await;
    go_to_about(&page).await;
    shows(&page, &dev, "2").await;
    // The request for the route's module says which run of the dev server
    // rendered the page.
    assert!(
        page.eval::<bool>(
            "performance.getEntriesByType('resource').some((e) => e.name.includes('/@oj-start/hmr/lazy') && /[?&]g=[0-9a-f]+-\\d+/.test(e.name))"
        )
        .await,
        "no request for a module carried the document's stamp: {:?}",
        page.eval::<Vec<String>>(
            "performance.getEntriesByType('resource').map((e) => e.name).filter((n) => n.includes('/hmr/'))"
        )
        .await
    );
    kept(&page, &dev).await;
    page.close().await;
}

/// The shared module already ran on the page (the home route reads it too)
/// and is edited alone: the update re-runs it in place, and the lazy route,
/// unchanged, shows its new value on the first visit.
#[tokio::test(flavor = "multi_thread")]
async fn an_unchanged_lazy_route_reads_a_module_its_update_re_ran() {
    let Some(app) = lazy_app(
        "lazy-stale-re-ran",
        "export const EVENTS = [\"a\", \"b\"];\n",
    ) else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    write(&root, INDEX, INDEX_READS);
    let dev = Server::oj_dev_with(&root, GATE);
    let page = open_home(&dev).await;

    change(
        &dev,
        &root,
        &[(EVENT, "export const EVENTS = [\"a\", \"b\", \"c\"];\n")],
    )
    .await;
    page.wait_for(
        "document.querySelector('#home')?.textContent === 'a,b,c'",
        WAIT,
    )
    .await;
    go_to_about(&page).await;
    shows(&page, &dev, "3").await;
    kept(&page, &dev).await;
    page.close().await;
}

/// The lazy route is preloaded from its hovered link after the update: the
/// click lands on the route with the new values, and nothing reloads.
#[tokio::test(flavor = "multi_thread")]
async fn a_lazy_route_preloaded_after_its_update_shows_it_at_the_click() {
    let Some(app) = lazy_app(
        "lazy-stale-preload",
        "export const EVENTS = [\"a\", \"b\"];\n",
    ) else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    write(&root, "src/router.tsx", ROUTER_PRELOAD);
    let dev = Server::oj_dev_with(&root, GATE);
    let page = open_home(&dev).await;

    change(
        &dev,
        &root,
        &[
            (
                EVENT,
                "export const EVENTS = [\"a\", \"b\"];\nexport const FEATURED = [\"a\"];\nexport const COUNT = 3;\n",
            ),
            (
                ABOUT,
                &about(
                    "EVENTS, FEATURED, COUNT",
                    "EVENTS.length + \"|\" + FEATURED.map((f) => f.toUpperCase()).join(\",\") + \"|\" + COUNT",
                ),
            ),
        ],
    )
    .await;
    heard(&page, &dev, 1).await;
    page.eval::<bool>(
        "(() => { const a = document.querySelector('#to-about'); \
         a.dispatchEvent(new MouseEvent('mouseover', { bubbles: true })); a.focus(); return true; })()",
    )
    .await;
    // The preload ran the route's component module (from the update the
    // page took, which carried it, or from the server).
    page.wait_for(
        "[...globalThis.__rolldown_runtime__.moduleCache.keys()].some((id) => id.includes('about.tsx?tsr-split=component'))",
        WAIT,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        page.eval::<String>("location.pathname + ' ' + String(window.__mark)")
            .await,
        "/ kept",
        "the preload moved or reloaded the page the user is on"
    );
    go_to_about(&page).await;
    shows(&page, &dev, "2|A|3").await;
    kept(&page, &dev).await;
    page.close().await;
}

/// The page shows `text` as the module imported from the layout's button.
async fn notes(page: &Page, dev: &Server, text: &str) {
    let shown = page
        .poll(
            &format!("{NOTE_SHOWN} === {}", serde_json::to_string(text).unwrap()),
            Duration::from_secs(40),
        )
        .await;
    assert!(
        shown,
        "the page never showed the note {text:?}: shows {:?}; page errors: {:?}; console: {:?}\n{}",
        page.eval::<String>(NOTE_SHOWN).await,
        page.errors(),
        page.console(),
        dev.output()
    );
}

async fn load_note(page: &Page) {
    page.eval::<bool>("(document.querySelector('#load').click(), true)")
        .await;
}

fn hot_updates(dev: &Server) -> usize {
    dev.output().matches("oj start: hot updated").count()
}

/// Waits for a "hot updated" line past `before` of them: the edit went to
/// the pages in place.
async fn hot_updated(dev: &Server, before: usize) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while hot_updates(dev) <= before {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the edit was not a hot update\n{}",
            dev.output()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The lazy route reads the shared module through a barrel, a JSON file by
/// a named import, a text file and a module that exports a component; all
/// but the text are edited before the page first visits the route.
#[tokio::test(flavor = "multi_thread")]
async fn an_unloaded_lazy_route_reads_a_barrel_json_and_a_component_as_edited() {
    let Some(app) = lazy_app(
        "lazy-import-kinds",
        "export const EVENTS = [\"a\", \"b\"];\n",
    ) else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    let barrel = "src/lib/barrel.ts";
    let data = "src/lib/data.json";
    let badge = "src/lib/Badge.tsx";
    write(&root, barrel, "export { EVENTS } from \"./event\";\n");
    write(&root, data, "{ \"title\": \"json-one\" }\n");
    write(&root, "src/lib/text.txt", "text-one\n");
    let badge_of = |text: &str| {
        format!("export function Badge() {{\n  return <b id=\"badge\">{text}</b>;\n}}\n")
    };
    write(&root, badge, &badge_of("badge-one"));
    write(
        &root,
        ABOUT,
        r#"import { createFileRoute } from "@tanstack/react-router";
import { EVENTS } from "../lib/barrel";
import { title } from "../lib/data.json";
import text from "../lib/text.txt?raw";
import { Badge } from "../lib/Badge";

export const Route = createFileRoute("/about")({
  component: AboutPage,
});

function AboutPage() {
  return (
    <p id="about">
      {EVENTS.join(",")}|{title}|{text.trim()}|<Badge />
    </p>
  );
}
"#,
    );
    let dev = Server::oj_dev_with(&root, GATE);
    let page = open_home(&dev).await;

    change(
        &dev,
        &root,
        &[
            (EVENT, "export const EVENTS = [\"a\", \"b\", \"c\"];\n"),
            (data, "{ \"title\": \"json-two\" }\n"),
            (badge, &badge_of("badge-two")),
        ],
    )
    .await;
    go_to_about(&page).await;
    shows(&page, &dev, "a,b,c|json-two|text-one|badge-two").await;
    kept(&page, &dev).await;

    // Loaded now, each takes its next edit in place.
    change(
        &dev,
        &root,
        &[
            (data, "{ \"title\": \"json-three\" }\n"),
            (badge, &badge_of("badge-three")),
        ],
    )
    .await;
    shows(&page, &dev, "a,b,c|json-three|text-one|badge-three").await;
    kept(&page, &dev).await;
    page.close().await;
}

/// One change the editor's gate holds edits the lazy route, which the page
/// has not loaded, and the shared module the home route shows. The page
/// goes to the route during the hold: it must not render the route's new
/// code against the shared module from before the change (the patch for
/// that one is held). The route waits for the flush, and then both show the
/// change, with no reload.
#[tokio::test(flavor = "multi_thread")]
async fn a_lazy_route_changed_in_a_held_change_waits_for_the_flush() {
    let Some(app) = lazy_app(
        "lazy-import-held",
        "export const EVENTS = [\"a\", \"b\"];\n",
    ) else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    write(&root, INDEX, INDEX_READS);
    write(&root, ABOUT, &about("EVENTS", "\"v1 \" + EVENTS.length"));
    let dev = Server::oj_dev_with(&root, GATE);
    let page = open_home(&dev).await;

    let before = taken_in(&dev);
    held(
        &dev,
        &root,
        &[
            (EVENT, "export const EVENTS = [\"a\", \"b\", \"c\"];\n"),
            (ABOUT, &about("EVENTS", "\"v2 \" + EVENTS.length")),
        ],
    )
    .await;
    took_in(&dev, before).await;
    go_to_about(&page).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        let shown = page.eval::<String>(SHOWN).await;
        assert!(
            !shown.starts_with("v2"),
            "the held change's route rendered during the hold: {shown:?}\n{}",
            dev.output()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    flush(&dev);
    shows(&page, &dev, "v2 3").await;
    kept(&page, &dev).await;
    page.close().await;
}

/// Notes, in the page, each url its runtime imports while a change is held
/// (the kept chunk it takes, the module it asks for again).
async fn watch_held_imports(page: &Page) {
    page.eval::<bool>(
        "(() => { const rt = globalThis.__rolldown_runtime__; const real = rt.ojImport.bind(rt); \
         globalThis.__heldImports = []; \
         rt.ojImport = (url) => { globalThis.__heldImports.push(url); return real(url); }; return true; })()",
    )
    .await;
}

/// Whether the page's runtime imported a url holding `part` during a hold.
const HELD_IMPORT: &str =
    "(part) => (globalThis.__heldImports ?? []).some((url) => url.includes(part))";

/// A held change edits only a module the page already runs (the home route
/// shows it). The lazy route, which reads that module and is not part of
/// the change, loads during the hold without waiting for the flush, against
/// the module as the page has it; the flush then updates both in place.
#[tokio::test(flavor = "multi_thread")]
async fn a_lazy_route_reading_a_held_module_the_page_runs_loads_during_the_hold() {
    let Some(app) = lazy_app(
        "lazy-import-held-util",
        "export const EVENTS = [\"a\", \"b\"];\n",
    ) else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    write(&root, INDEX, INDEX_READS);
    let dev = Server::oj_dev_with(&root, GATE);
    let page = open_home(&dev).await;

    watch_held_imports(&page).await;
    let before = taken_in(&dev);
    held(
        &dev,
        &root,
        &[(EVENT, "export const EVENTS = [\"a\", \"b\", \"c\"];\n")],
    )
    .await;
    took_in(&dev, before).await;
    go_to_about(&page).await;
    let loaded = page
        .poll(&format!("{SHOWN} === '2'"), Duration::from_secs(15))
        .await;
    assert!(
        loaded,
        "the lazy route did not load during the hold: shows {:?}; page errors: {:?}\n{}",
        page.eval::<String>(SHOWN).await,
        page.errors(),
        dev.output()
    );
    // Through the chunk kept for this page, not by waiting.
    assert!(
        page.eval::<bool>(&format!(
            "({HELD_IMPORT})('lazy_held_') && !({HELD_IMPORT})('held=1')"
        ))
        .await,
        "the page did not take the kept chunk: {:?}",
        page.eval::<Vec<String>>("globalThis.__heldImports").await
    );

    flush(&dev);
    shows(&page, &dev, "3").await;
    kept(&page, &dev).await;
    page.close().await;
}

/// A held change edits only a module the page has not loaded. Imported
/// during the hold, it must not show the held code: the import waits for
/// the flush.
#[tokio::test(flavor = "multi_thread")]
async fn a_module_only_a_held_change_edits_is_not_imported_before_the_flush() {
    let Some(app) = lazy_app(
        "lazy-import-held-unloaded",
        "export const EVENTS = [\"a\", \"b\"];\n",
    ) else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    let dev = Server::oj_dev_with(&root, GATE);
    let page = open_home(&dev).await;

    watch_held_imports(&page).await;
    let before = taken_in(&dev);
    held(&dev, &root, &[(NOTE, "export const note = \"note v2\";\n")]).await;
    took_in(&dev, before).await;
    load_note(&page).await;
    assert!(
        page.poll(&format!("({HELD_IMPORT})('held=1')"), WAIT).await,
        "the page did not ask to wait for the flush"
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        assert_eq!(
            page.eval::<String>(NOTE_SHOWN).await,
            "none",
            "the held module was imported during the hold\n{}",
            dev.output()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    flush(&dev);
    notes(&page, &dev, "note v2").await;
    kept(&page, &dev).await;
    page.close().await;
}

/// The flush of a held change that needs a reload (it adds a route): an
/// import waiting for it ends with the page, and the reloaded page imports
/// the module as changed.
#[tokio::test(flavor = "multi_thread")]
async fn an_import_waiting_for_a_flush_that_reloads_is_current_after_it() {
    let Some(app) = lazy_app(
        "lazy-import-held-reload",
        "export const EVENTS = [\"a\", \"b\"];\n",
    ) else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    let dev = Server::oj_dev_with(&root, GATE);
    let page = open_home(&dev).await;

    let extra = "src/routes/extra.tsx";
    watch_held_imports(&page).await;
    let before = taken_in(&dev);
    held(
        &dev,
        &root,
        &[
            (NOTE, "export const note = \"note v2\";\n"),
            (
                extra,
                "import { createFileRoute } from \"@tanstack/react-router\";\n\nexport const Route = createFileRoute(\"/extra\")({\n  component: () => <p id=\"extra\">extra</p>,\n});\n",
            ),
        ],
    )
    .await;
    took_in(&dev, before).await;
    load_note(&page).await;
    assert!(
        page.poll(&format!("({HELD_IMPORT})('held=1')"), WAIT).await,
        "the page did not ask to wait for the flush"
    );
    assert_eq!(page.eval::<String>(NOTE_SHOWN).await, "none");

    flush(&dev);
    hot_ready(&page).await;
    load_note(&page).await;
    notes(&page, &dev, "note v2").await;
    page.close().await;
}

/// Two tabs and one held change, to a module the first tab has imported and
/// the second has not. Both go to a lazy route that reads it during the
/// hold: the first loads it at once, against the module as it runs it; the
/// second, which would run the module's held code, waits. The flush brings
/// both to the change, with no reload.
#[tokio::test(flavor = "multi_thread")]
async fn each_tab_is_answered_by_what_it_runs_during_a_hold() {
    let Some(app) = lazy_app(
        "lazy-import-held-tabs",
        "export const EVENTS = [\"a\", \"b\"];\n",
    ) else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    write(
        &root,
        ABOUT,
        r#"import { createFileRoute } from "@tanstack/react-router";
import { note } from "../lib/note";

export const Route = createFileRoute("/about")({
  component: AboutPage,
});

function AboutPage() {
  return <p id="about">{note}</p>;
}
"#,
    );
    let dev = Server::oj_dev_with(&root, GATE);
    let runs = open_home(&dev).await;
    load_note(&runs).await;
    notes(&runs, &dev, "note v1").await;
    let lacks = open_home(&dev).await;

    watch_held_imports(&runs).await;
    watch_held_imports(&lacks).await;
    let before = taken_in(&dev);
    held(&dev, &root, &[(NOTE, "export const note = \"note v2\";\n")]).await;
    took_in(&dev, before).await;
    go_to_about(&runs).await;
    go_to_about(&lacks).await;
    assert!(
        runs.poll(&format!("{SHOWN} === 'note v1'"), Duration::from_secs(15))
            .await,
        "the tab that runs the module did not load the route during the hold: {:?}\n{}",
        runs.eval::<String>(SHOWN).await,
        dev.output()
    );
    assert!(
        lacks
            .poll(&format!("({HELD_IMPORT})('held=1')"), WAIT)
            .await,
        "the tab that never ran the module did not ask to wait for the flush"
    );
    assert_eq!(
        lacks.eval::<String>(SHOWN).await,
        "",
        "the tab that never ran the module rendered the route during the hold"
    );

    flush(&dev);
    shows(&runs, &dev, "note v2").await;
    shows(&lacks, &dev, "note v2").await;
    kept(&runs, &dev).await;
    kept(&lacks, &dev).await;
    lacks.close().await;
    runs.close().await;
}

/// A lazy route the held change does not touch loads at once during the
/// hold.
#[tokio::test(flavor = "multi_thread")]
async fn a_lazy_route_a_held_change_left_alone_loads_during_the_hold() {
    let Some(app) = lazy_app(
        "lazy-import-held-other",
        "export const EVENTS = [\"a\", \"b\"];\n",
    ) else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    let dev = Server::oj_dev_with(&root, GATE);
    let page = open_home(&dev).await;

    let before = taken_in(&dev);
    held(&dev, &root, &[(INDEX, &index("home v2"))]).await;
    took_in(&dev, before).await;
    go_to_about(&page).await;
    shows(&page, &dev, "2").await;
    flush(&dev);
    kept(&page, &dev).await;
    page.close().await;
}

/// Two tabs: one has the lazy route loaded, the other never visited it. The
/// route's edit patches the first in place; the second, going to the route
/// afterwards, shows the edit too. Neither reloads.
#[tokio::test(flavor = "multi_thread")]
async fn a_tab_that_never_loaded_the_lazy_route_shows_what_the_other_was_patched_to() {
    let Some(app) = lazy_app(
        "lazy-import-tabs",
        "export const EVENTS = [\"a\", \"b\"];\n",
    ) else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    let dev = Server::oj_dev_with(&root, GATE);
    let loaded = open_home(&dev).await;
    go_to_about(&loaded).await;
    shows(&loaded, &dev, "2").await;
    let other = open_home(&dev).await;

    change(
        &dev,
        &root,
        &[
            (EVENT, "export const EVENTS = [\"a\", \"b\", \"c\"];\n"),
            (ABOUT, &about("EVENTS", "\"v2 \" + EVENTS.length")),
        ],
    )
    .await;
    shows(&loaded, &dev, "v2 3").await;
    go_to_about(&other).await;
    shows(&other, &dev, "v2 3").await;
    kept(&loaded, &dev).await;
    kept(&other, &dev).await;
    other.close().await;
    loaded.close().await;
}

/// Plain `oj dev` (no gate). A page open since before the edit imports a
/// module from a handler for the first time: it gets the module as edited,
/// with no reload; edited again, the next import gets that.
#[tokio::test(flavor = "multi_thread")]
async fn an_open_page_first_imports_a_hot_updated_module_as_it_is_now() {
    let Some(app) = lazy_app(
        "lazy-import-note",
        "export const EVENTS = [\"a\", \"b\"];\n",
    ) else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    let dev = Server::oj_dev(&root);
    let page = open_home(&dev).await;

    let before = hot_updates(&dev);
    write(&root, NOTE, "export const note = \"note v2\";\n");
    hot_updated(&dev, before).await;
    kept(&page, &dev).await;
    load_note(&page).await;
    notes(&page, &dev, "note v2").await;

    let before = hot_updates(&dev);
    write(&root, NOTE, "export const note = \"note v3\";\n");
    hot_updated(&dev, before).await;
    load_note(&page).await;
    notes(&page, &dev, "note v3").await;
    kept(&page, &dev).await;
    assert!(page.errors().is_empty(), "page errors: {:?}", page.errors());
    page.close().await;
}

/// A module behind an `import()` in a click handler, not loaded yet, imports
/// a module the page runs. That module is edited (the page's hot update
/// carries the unloaded importer too), then the unloaded module alone (an
/// update the page has no use for), then the page imports it: it gets the
/// module as edited, as a page under Vite does (the change stops at a module
/// no page loaded, and the import is a request answered with current code).
#[tokio::test(flavor = "multi_thread")]
async fn a_module_edited_after_a_module_it_imports_is_imported_as_edited() {
    let Some(app) = lazy_app("lazy-import-after-dep", "export const EVENTS = [\"a\"];\n") else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    write(&root, INDEX, INDEX_READS);
    let note = |v: &str| {
        format!("import {{ EVENTS }} from \"./event\";\nexport const note = \"note {v}:\" + EVENTS.join(\"\");\n")
    };
    write(&root, NOTE, &note("v1"));
    let dev = Server::oj_dev(&root);
    let page = open_home(&dev).await;

    let before = hot_updates(&dev);
    write(&root, EVENT, "export const EVENTS = [\"b\"];\n");
    hot_updated(&dev, before).await;
    assert!(
        page.poll("document.querySelector('#home')?.textContent === 'b'", WAIT)
            .await,
        "the home route did not take the edit of the module it reads\n{}",
        dev.output()
    );
    let before = hot_updates(&dev);
    write(&root, NOTE, &note("v2"));
    hot_updated(&dev, before).await;
    load_note(&page).await;
    notes(&page, &dev, "note v2:b").await;

    // And once more, now that the page runs it.
    let before = hot_updates(&dev);
    write(&root, NOTE, &note("v3"));
    hot_updated(&dev, before).await;
    load_note(&page).await;
    notes(&page, &dev, "note v3:b").await;
    kept(&page, &dev).await;
    assert!(page.errors().is_empty(), "page errors: {:?}", page.errors());
    page.close().await;
}

/// An `import()` with an import attribute (`with { type: "json" }`) of a
/// file the page has not loaded, edited before the page imports it: the
/// page gets the file as edited, as with any other `import()`.
#[tokio::test(flavor = "multi_thread")]
async fn an_import_with_an_attribute_is_answered_as_the_file_is_now() {
    let Some(app) = lazy_app("lazy-import-attribute", "export const EVENTS = [\"a\"];\n") else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    let with_attribute = ROOT.replace(
        "import(\"../lib/note\").then((m) => setNote(m.note),",
        "import(\"../lib/data.json\", { with: { type: \"json\" } }).then((m) => setNote(m.default.note),",
    );
    assert_ne!(with_attribute, ROOT);
    write(&root, "src/routes/__root.tsx", &with_attribute);
    write(&root, "src/lib/data.json", "{ \"note\": \"data v1\" }\n");
    let dev = Server::oj_dev(&root);
    // The entry asks for it by id, like an `import()` with no attribute.
    assert!(proxy_of(&dev, "src/lib/data.json").ends_with("data.json?rolldown-lazy=1"));
    let page = open_home(&dev).await;

    let before = hot_updates(&dev);
    write(&root, "src/lib/data.json", "{ \"note\": \"data v2\" }\n");
    hot_updated(&dev, before).await;
    load_note(&page).await;
    notes(&page, &dev, "data v2").await;
    kept(&page, &dev).await;
    assert!(page.errors().is_empty(), "page errors: {:?}", page.errors());
    page.close().await;
}

/// Plain `oj dev`, a file-routed app: the router's code splitter moves a
/// route's component into `route.tsx?tsr-split=component`. A page open on
/// the home route has not loaded it; a module only that component imports is
/// edited (a hot update), and the page follows its link to the route: it
/// shows the route as it is now, with no reload.
#[tokio::test(flavor = "multi_thread")]
async fn an_open_page_goes_to_a_lazy_route_hot_updated_since_as_it_is_now() {
    let Some(app) = lazy_app("lazy-import-route", "export const EVENTS = [\"one\"];\n") else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    write(&root, ABOUT, &about("EVENTS", "EVENTS.join(\",\")"));
    let dev = Server::oj_dev(&root);
    let page = open_home(&dev).await;
    // The route's component is a module of its own, behind an `import()`
    // the entry asks the dev server for by id.
    let (_, entry) = dev.request("GET", "/@oj-start/client-entry.js", &[]);
    assert!(
        entry.contains("about.tsx?tsr-split=component?rolldown-lazy=1"),
        "the route's split-off component is not behind a lazy boundary"
    );

    let before = hot_updates(&dev);
    write(&root, EVENT, "export const EVENTS = [\"two\"];\n");
    hot_updated(&dev, before).await;
    kept(&page, &dev).await;
    go_to_about(&page).await;
    shows(&page, &dev, "two").await;
    kept(&page, &dev).await;
    page.close().await;
}

/// The module behind an `import()` does not compile when the page asks for
/// it (a syntax error mid-edit): the import rejects, the page stays. Once
/// the file is fixed the next import asks again and gets the module.
#[tokio::test(flavor = "multi_thread")]
async fn an_import_that_failed_to_compile_is_asked_for_again() {
    let Some(app) = lazy_app(
        "lazy-import-failed",
        "export const EVENTS = [\"a\", \"b\"];\n",
    ) else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    let dev = Server::oj_dev(&root);
    let page = open_home(&dev).await;

    write(&root, NOTE, "export const note = \"broken;\n");
    let deadline = tokio::time::Instant::now() + WAIT;
    while !dev.output().contains("client bundle failed") {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the broken edit never failed the bundle\n{}",
            dev.output()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // The overlay shows the error of the file, with the file's name.
    let overlay = "(document.querySelector('vite-error-overlay')?.shadowRoot?.textContent ?? '')";
    assert!(
        page.poll(&format!("{overlay}.includes('note.ts')"), WAIT)
            .await,
        "the overlay does not show the broken file: {:?}",
        page.eval::<String>(overlay).await
    );
    // The import of the broken module rejects at once with the module's
    // own error, as a request for a module that does not transform does
    // under Vite.
    let proxy = proxy_of(&dev, "src/lib/note.ts");
    let outcome = format!(
        "Promise.race([globalThis.__rolldown_runtime__.requestLazy('src/lib/note.ts', {proxy:?}).then(() => 'resolved', (e) => 'rejected: ' + e.message), new Promise((r) => setTimeout(r, 8000, 'pending'))])"
    );
    let rejected = |outcome: &str| outcome.starts_with("rejected: ") && outcome.contains("note.ts");
    let first = page.eval::<String>(&outcome).await;
    assert!(
        rejected(&first),
        "the import of a module that does not compile: {first}"
    );
    // The answer is a module and nothing else: as a classic script (a
    // `<script>` on a page of another origin) it does not parse.
    let (status, body) = dev.request("GET", &lazy_url(&proxy, "no-socket"), &[]);
    assert!(
        status == 200 && body.contains("showBuildError") && body.trim_end().ends_with("export {};"),
        "{status} {body}"
    );
    load_note(&page).await;
    assert!(
        page.poll(&format!("{NOTE_SHOWN}.startsWith('failed: ')"), WAIT)
            .await,
        "the app's own import: {:?}",
        page.eval::<String>(NOTE_SHOWN).await
    );
    // A module the broken file has nothing to do with is not failed for
    // it: its import waits for a bundle that builds (the bundler compiles
    // nothing while a module of the graph is broken).
    go_to_about(&page).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(page.errors().is_empty(), "page errors: {:?}", page.errors());
    // Waiting: the request for the route's module is not answered, and
    // the route has neither rendered nor failed.
    let state = "JSON.stringify([performance.getEntriesByType('resource').filter((e) => e.name.includes('/@oj-start/hmr/lazy') && decodeURIComponent(e.name).includes('about.tsx')).map((e) => e.responseEnd > 0), document.querySelector('#about') === null])";
    let waiting = page.eval::<String>(state).await;
    assert!(
        waiting == "[[],true]",
        "the import of a module the error is not about did not wait: {waiting}"
    );

    write(&root, NOTE, "export const note = \"note fixed\";\n");
    notes_once_current(&page, &dev, "note fixed").await;
    shows_despite_overlay(&page, &dev, "2").await;
    assert!(
        page.poll(&format!("{overlay} === ''"), WAIT).await,
        "the overlay stayed up after the fix: {:?}; mark {:?}; console {:?}\n{}",
        page.eval::<String>(overlay)
            .await
            .chars()
            .take(80)
            .collect::<String>(),
        page.eval::<String>("String(window.__mark)").await,
        page.console()
            .iter()
            .map(|l| l.chars().take(160).collect::<String>())
            .collect::<Vec<_>>(),
        dev.output()
            .lines()
            .filter(|l| l.contains("oj start:") || l.contains("bundl"))
            .map(|l| l.chars().take(140).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    );
    let said: Vec<String> = page
        .console()
        .into_iter()
        .chain(page.errors())
        .filter(|line| line.contains("last full build failed"))
        .collect();
    assert!(
        said.is_empty(),
        "an import failed for the bundle's state: {said:?}"
    );
    kept(&page, &dev).await;
    page.close().await;
}

/// A module the page runs does not compile (a syntax error mid-edit) while
/// imports of modules the page has not loaded wait for the bundle to build
/// again. The fix is saved: whichever reads it first, an import that was
/// waiting or the bundler's own watcher, the page gets the fixed module, as
/// a page under Vite is hot-updated with it.
#[tokio::test(flavor = "multi_thread")]
async fn a_fix_saved_while_imports_wait_on_a_broken_build_reaches_the_page() {
    let Some(app) = lazy_app("lazy-import-fix-race", "export const EVENTS = [\"a\"];\n") else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    write(&root, INDEX, INDEX_READS);
    let dev = Server::oj_dev(&root);
    let page = open_home(&dev).await;
    let proxy = proxy_of(&dev, "src/lib/note.ts");
    let port = dev.port;

    for round in 0..4 {
        write(&root, EVENT, "export const EVENTS = [\"broken;\n");
        let failed = dev.output().matches("client bundle failed").count();
        let deadline = tokio::time::Instant::now() + WAIT;
        while dev.output().matches("client bundle failed").count() == failed {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the broken edit never failed the bundle\n{}",
                dev.output()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // Imports of a module the error is not about, from pages of their
        // own: each waits, asking the bundler again and again.
        let waiting: Vec<_> = (0..24)
            .map(|n| {
                let proxy = proxy.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(n * 6));
                    let _ = std::process::Command::new("curl")
                        .args(["-s", "-o", "/dev/null", "-m", "20"])
                        .arg(format!(
                            "http://127.0.0.1:{port}{}",
                            lazy_url(&proxy, &format!("waiting-{round}-{n}"))
                        ))
                        .status();
                })
            })
            .collect();
        tokio::time::sleep(Duration::from_millis(600)).await;
        let fixed = format!("fixed{round}");
        write(
            &root,
            EVENT,
            &format!("export const EVENTS = [\"{fixed}\"];\n"),
        );
        assert!(
            page.poll(
                &format!("document.querySelector('#home')?.textContent === '{fixed}'"),
                Duration::from_secs(20)
            )
            .await,
            "round {round}: the page never ran the fixed module: shows {:?}\n{}",
            page.eval::<String>("document.querySelector('#home')?.textContent ?? ''")
                .await,
            dev.output()
                .lines()
                .filter(|l| l.contains("oj start:"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        for thread in waiting {
            let _ = thread.join();
        }
    }
    page.close().await;
}

/// [`shows`], for a page whose console holds the build error an overlay
/// shows.
async fn shows_despite_overlay(page: &Page, dev: &Server, text: &str) {
    let shown = page
        .poll(
            &format!("{SHOWN} === {}", serde_json::to_string(text).unwrap()),
            Duration::from_secs(40),
        )
        .await;
    assert!(
        shown,
        "the page never showed {text:?}: shows {:?}; page errors: {:?}\n{}",
        page.eval::<String>(SHOWN).await,
        page.errors(),
        dev.output()
    );
}

/// Under a dev `base` the page asks for the module under it.
#[tokio::test(flavor = "multi_thread")]
async fn an_import_is_asked_for_under_the_dev_base() {
    let Some(app) = lazy_app(
        "lazy-import-base",
        "export const EVENTS = [\"a\", \"b\"];\n",
    ) else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    let config = std::fs::read_to_string(root.join("vite.config.ts")).unwrap();
    let based = config.replacen(
        "export default defineConfig({",
        "export default defineConfig({\n  base: \"/app/\",",
        1,
    );
    assert_ne!(based, config, "the fixture's config moved");
    write(&root, "vite.config.ts", &based);
    let dev = Server::oj_dev(&root);
    let page = open_at(&dev, "/app/").await;

    let before = hot_updates(&dev);
    write(&root, NOTE, "export const note = \"note v2\";\n");
    hot_updated(&dev, before).await;
    load_note(&page).await;
    notes(&page, &dev, "note v2").await;
    assert!(
        page.eval::<bool>(
            "performance.getEntriesByType('resource').some((e) => new URL(e.name).pathname === '/app/@oj-start/hmr/lazy')"
        )
        .await,
        "the module was not asked for under the base"
    );
    kept(&page, &dev).await;
    page.close().await;
}

/// The same app booted twice: the second boot restores the client bundle
/// from the cache and runs no bundler until the first edit.
fn restored(root: &Path) -> Server {
    {
        let first = Server::oj_dev(root);
        let (status, _) = first.request("GET", "/", &[]);
        assert_eq!(status, 200, "the first boot\n{}", first.output());
        let deadline = std::time::Instant::now() + WAIT;
        while !first.output().contains("client bundle cached")
            && !first.output().contains("client bundle restored from cache")
        {
            assert!(
                std::time::Instant::now() < deadline,
                "the first boot cached no bundle\n{}",
                first.output()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    let dev = Server::oj_dev(root);
    let (status, _) = dev.request("GET", "/", &[]);
    assert_eq!(status, 200, "the second boot\n{}", dev.output());
    assert!(
        dev.output().contains("client bundle restored from cache"),
        "the second boot did not restore the bundle\n{}",
        dev.output()
    );
    dev
}

/// A boot that restored the bundle from the cache answers an `import()`
/// from the restored chunks: the page gets the module, and no bundler was
/// started for it.
#[tokio::test(flavor = "multi_thread")]
async fn a_restored_boot_answers_an_import_without_bundling() {
    let Some(app) = lazy_app(
        "lazy-import-restored",
        "export const EVENTS = [\"a\", \"b\"];\n",
    ) else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    let dev = restored(&root);
    let page = open_home(&dev).await;
    load_note(&page).await;
    notes(&page, &dev, "note v1").await;
    go_to_about(&page).await;
    shows(&page, &dev, "2").await;
    kept(&page, &dev).await;
    assert!(
        !dev.output().contains("client bundling"),
        "an import started the bundler on a restored boot\n{}",
        dev.output()
    );
    page.close().await;
}

/// A restored boot, then an edit of a module the page has not imported: the
/// edit starts the bundler (the page reloads onto its bundle, as it does for
/// any first edit after a restored boot), and the import that follows gets
/// the module as edited.
#[tokio::test(flavor = "multi_thread")]
async fn a_restored_boot_edited_before_an_import_answers_it_as_edited() {
    let Some(app) = lazy_app(
        "lazy-import-restored-edit",
        "export const EVENTS = [\"a\", \"b\"];\n",
    ) else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    let dev = restored(&root);
    let page = open_home(&dev).await;

    write(&root, NOTE, "export const note = \"note v2\";\n");
    hot_ready(&page).await;
    load_note(&page).await;
    notes(&page, &dev, "note v2").await;

    // The bundler runs now: the next edit of the module, loaded, is a hot
    // update, and so is one of the lazy route before its first visit.
    let before = hot_updates(&dev);
    write(&root, NOTE, "export const note = \"note v3\";\n");
    hot_updated(&dev, before).await;
    load_note(&page).await;
    notes(&page, &dev, "note v3").await;
    let before = hot_updates(&dev);
    write(
        &root,
        EVENT,
        "export const EVENTS = [\"a\", \"b\", \"c\"];\n",
    );
    hot_updated(&dev, before).await;
    go_to_about(&page).await;
    shows(&page, &dev, "3").await;
    kept(&page, &dev).await;
    page.close().await;
}

/// A restored boot, an import answered from the restored chunks, then an
/// edit of that module: the page comes to show the edit (through the reload
/// a first edit after a restored boot makes), and imports it as edited.
#[tokio::test(flavor = "multi_thread")]
async fn a_module_imported_on_a_restored_boot_and_then_edited_is_current() {
    let Some(app) = lazy_app(
        "lazy-import-restored-loaded",
        "export const EVENTS = [\"a\", \"b\"];\n",
    ) else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    let dev = restored(&root);
    let page = open_home(&dev).await;
    load_note(&page).await;
    notes(&page, &dev, "note v1").await;
    go_to_about(&page).await;
    shows(&page, &dev, "2").await;

    write(
        &root,
        EVENT,
        "export const EVENTS = [\"a\", \"b\", \"c\"];\n",
    );
    shows(&page, &dev, "3").await;
    write(&root, NOTE, "export const note = \"note v2\";\n");
    notes_once_current(&page, &dev, "note v2").await;
    assert!(page.errors().is_empty(), "page errors: {:?}", page.errors());
    page.close().await;
}

/// The proxy id the entry asks the dev server for, for the module whose id
/// ends in `module`.
fn proxy_of(dev: &Server, module: &str) -> String {
    let (_, entry) = dev.request("GET", "/@oj-start/client-entry.js", &[]);
    let end = entry
        .find(&format!("{module}?rolldown-lazy=1\""))
        .unwrap_or_else(|| panic!("the entry asks for no {module}"))
        + module.len()
        + "?rolldown-lazy=1".len();
    let start = entry[..end].rfind('"').unwrap() + 1;
    entry[start..end].to_string()
}

/// The running engine's tag, as the client bundle's runtime carries it: the
/// start of the runtime id of every page of that bundle.
fn engine_tag(dev: &Server) -> String {
    let (_, entry) = dev.request("GET", "/@oj-start/client-entry.js", &[]);
    let tag_in = |code: &str| {
        let after = &code[code.find("new OjDevRuntime(")? + "new OjDevRuntime(".len()..];
        let tag: String = after
            .trim_start_matches(['`', '$', '{', '"'])
            .chars()
            .take_while(char::is_ascii_hexdigit)
            .collect();
        (!tag.is_empty()).then_some(tag)
    };
    // The runtime is in the entry, or in the chunk the entry imports it from.
    tag_in(&entry)
        .or_else(|| {
            let at = entry.find("rolldown-runtime")?;
            let end = at + entry[at..].find('"')?;
            let (_, runtime) = dev.request("GET", &format!("/@oj-start/{}", &entry[at..end]), &[]);
            tag_in(&runtime)
        })
        .expect("the client bundle's runtime names its engine")
}

fn encoded(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn lazy_url(proxy: &str, client: &str) -> String {
    format!(
        "/@oj-start/hmr/lazy?id={}&clientId={client}",
        encoded(proxy)
    )
}

/// An `import()` of something that does not resolve fails the bundle as an
/// unresolved import that names it and its importer, in the first build and
/// in an edit, as Vite reports `Failed to resolve import "x" from "file"`.
#[test]
fn an_import_that_does_not_resolve_names_itself_and_its_importer() {
    for (case, missing) in [("relative", "./no-such-file"), ("bare", "no-such-dep-typo")] {
        let broken = format!(
            "export const note = \"note v1\";\nexport const more = () => import(\"{missing}\");\n"
        );
        let named = |out: &str| {
            let out = out.to_lowercase();
            out.contains(&format!("failed to resolve import \"{missing}\""))
                && out.contains("note.ts")
        };
        let waits = |dev: &Server, what: &str| {
            let deadline = std::time::Instant::now() + WAIT;
            while !named(&dev.output()) {
                assert!(
                    std::time::Instant::now() < deadline,
                    "{case}, {what}: the unresolved import was not named\n{}",
                    dev.output()
                );
                std::thread::sleep(Duration::from_millis(50));
            }
            assert!(
                !dev.output().contains("oj:start-lazy-imports"),
                "{case}, {what}: reported as a plugin's error\n{}",
                dev.output()
            );
        };

        // In the first build.
        let Some(app) = lazy_app(
            &format!("lazy-import-unresolved-boot-{case}"),
            "export const EVENTS = [\"a\"];\n",
        ) else {
            return;
        };
        write(&app.0, NOTE, &broken);
        let dev = Server::oj_dev(&app.0);
        let _ = dev.request("GET", "/", &[]);
        waits(&dev, "first build");
        drop(dev);

        // In an edit.
        let Some(app) = lazy_app(
            &format!("lazy-import-unresolved-edit-{case}"),
            "export const EVENTS = [\"a\"];\n",
        ) else {
            return;
        };
        let dev = Server::oj_dev(&app.0);
        let (status, _) = dev.request("GET", "/@oj-start/client-entry.js", &[]);
        assert_eq!(status, 200, "{}", dev.output());
        write(&app.0, NOTE, &broken);
        waits(&dev, "edit");
    }
}

/// A lazily imported module with top-level `await`: the import settles when
/// the module has finished evaluating, and rejects if it throws, as it does
/// in a browser.
#[tokio::test(flavor = "multi_thread")]
async fn an_import_waits_for_the_modules_top_level_await() {
    let Some(app) = lazy_app("lazy-import-tla", "export const EVENTS = [\"a\", \"b\"];\n") else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    write(
        &root,
        NOTE,
        "export const note = await new Promise((resolve) => setTimeout(() => resolve(\"awaited\"), 300));\n",
    );
    let dev = Server::oj_dev(&root);
    let page = open_home(&dev).await;
    load_note(&page).await;
    notes(&page, &dev, "awaited").await;

    let before = hot_updates(&dev);
    write(
        &root,
        NOTE,
        "await new Promise((resolve) => setTimeout(resolve, 100));\nthrow new Error(\"tla threw\");\nexport const note = \"never\";\n",
    );
    hot_updated(&dev, before).await;
    notes_once_current(&page, &dev, "failed: tla threw").await;
    kept(&page, &dev).await;
    page.close().await;
}

/// One edit gives a module the page runs a new export and makes the lazy
/// route call it. The page goes to the route when its hot update is still
/// on its way (held back here): the route's module must not run before the
/// page applied the update it was compiled against.
#[tokio::test(flavor = "multi_thread")]
async fn a_lazy_module_does_not_run_ahead_of_the_update_it_was_compiled_against() {
    let Some(app) = lazy_app(
        "lazy-import-order",
        "export const EVENTS = [\"a\", \"b\"];\n",
    ) else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    write(&root, INDEX, INDEX_READS);
    let dev = Server::oj_dev(&root);
    let page = open_home(&dev).await;
    // The page's hot updates reach it two seconds late.
    page.eval::<bool>(
        "(() => { const rt = globalThis.__rolldown_runtime__; const apply = rt.ojApply.bind(rt); \
         rt.ojApply = (frame) => setTimeout(() => apply(frame), 2000); return true; })()",
    )
    .await;

    let before = hot_updates(&dev);
    write(
        &root,
        EVENT,
        "export const EVENTS = [\"a\", \"b\"];\nexport const count = () => EVENTS.length + 1;\n",
    );
    write(&root, ABOUT, &about("count", "\"v2 \" + count()"));
    hot_updated(&dev, before).await;
    go_to_about(&page).await;
    shows(&page, &dev, "v2 3").await;
    kept(&page, &dev).await;
    page.close().await;
}

/// A request for a module costs the dev server no build, whatever it asks
/// for, and a page with no socket open is answered with the module alone
/// (not everything it imports).
#[test]
fn a_request_for_a_module_starts_no_build_and_answers_a_page_with_no_socket() {
    let Some(app) = lazy_app(
        "lazy-import-request",
        "export const EVENTS = [\"a\", \"b\"];\n",
    ) else {
        return;
    };
    // The module imports a package the entry holds too: an answer for a
    // page the bundler did not know would carry the package as well.
    write(
        &app.0,
        NOTE,
        "import { version } from \"react\";\nexport const note = \"note v1 \" + version;\n",
    );
    let dev = Server::oj_dev(&app.0);
    let proxy = proxy_of(&dev, "src/lib/note.ts");
    let builds = |dev: &Server| dev.output().matches("client bundling").count();
    let built = builds(&dev);

    // A page of the engine's bundle: its runtime id starts with the
    // engine's tag.
    let page = format!("{}.no-socket", engine_tag(&dev));
    let (status, body) = dev.request("GET", &lazy_url(&proxy, &page), &[]);
    assert_eq!(status, 200, "{body}");
    let (status, _) = dev.request("POST", &lazy_url(&proxy, &page), &[]);
    assert_eq!(status, 405, "only a GET is answered");
    assert!(
        body.contains("note v1") && body.len() < 4096,
        "the module without what the entry already holds: {} bytes",
        body.len()
    );
    // A page of some other bundle holds nothing the engine knows of: its
    // answer carries the package too.
    let (status, whole) = dev.request("GET", &lazy_url(&proxy, "no-socket"), &[]);
    assert!(
        status == 200 && whole.contains("note v1") && whole.len() > body.len(),
        "{status}: {} bytes against {}",
        whole.len(),
        body.len()
    );

    // Only the id of an `import()`'s boundary is answered: not the module
    // behind it under its own id, nor any other module of the bundle.
    let module = proxy
        .strip_suffix("?rolldown-lazy=1")
        .expect("a proxy id")
        .to_string();
    for id in [
        "/etc/passwd?rolldown-lazy=1",
        "garbage",
        &format!("{proxy}x"),
        &module,
    ] {
        let (status, body) = dev.request("GET", &lazy_url(id, "no-socket"), &[]);
        assert_eq!(status, 404, "{id}: {body}");
    }
    // A page another run of the dev server rendered says so in its
    // request, socket or not, and is told to reload.
    let other_run = format!("{}&g=1-0", lazy_url(&proxy, "old-tab"));
    let (status, body) = dev.request("GET", &other_run, &[]);
    assert!(
        status == 200 && body.contains("reloadBehind") && !body.contains("note v1"),
        "{status} {body}"
    );
    assert_eq!(
        builds(&dev),
        built,
        "a request started a build\n{}",
        dev.output()
    );
    drop(dev);

    // The same on a boot restored from the cache, which runs no bundler.
    let dev = restored(&app.0);
    let (status, body) = dev.request("GET", &lazy_url(&proxy, "no-socket"), &[]);
    assert!(
        status == 200 && body.starts_with("import \"../"),
        "{status} {body}"
    );
    let (status, body) = dev.request("GET", &other_run, &[]);
    assert!(
        status == 200 && body.contains("reloadBehind") && !body.contains("import \"../"),
        "{status} {body}"
    );
    for id in ["/etc/passwd?rolldown-lazy=1", "garbage"] {
        let (status, body) = dev.request("GET", &lazy_url(id, "no-socket"), &[]);
        assert_eq!(status, 404, "{id}: {body}");
    }
    assert_eq!(
        builds(&dev),
        0,
        "a request started a build\n{}",
        dev.output()
    );
}

/// A module with nothing to run (types only) imports as an empty module on
/// a restored boot too.
#[tokio::test(flavor = "multi_thread")]
async fn a_restored_boot_imports_a_module_with_no_code() {
    let Some(app) = lazy_app(
        "lazy-import-restored-empty",
        "export const EVENTS = [\"a\"];\n",
    ) else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    write(&root, NOTE, "export type Note = string;\n");
    let dev = restored(&root);
    let page = open_home(&dev).await;
    load_note(&page).await;
    // `m.note` is undefined: the span is empty, and no import failed.
    notes(&page, &dev, "").await;
    assert!(page.errors().is_empty(), "page errors: {:?}", page.errors());
    kept(&page, &dev).await;
    page.close().await;
}

/// An `import()` the dev server cannot answer (no such module) rejects, and
/// the page stays: it is not taken for a chunk gone from the server, which
/// reloads the page.
#[tokio::test(flavor = "multi_thread")]
async fn an_import_the_server_does_not_answer_rejects_without_a_reload() {
    let Some(app) = lazy_app("lazy-import-rejects", "export const EVENTS = [\"a\"];\n") else {
        return;
    };
    if browser::skip() {
        return;
    }
    let dev = Server::oj_dev(&app.0);
    let page = open_home(&dev).await;
    // Left uncaught, as an app's own `import()` may be; its outcome is
    // kept on the side.
    page.eval::<bool>(
        "(() => { const asked = globalThis.__rolldown_runtime__.requestLazy('src/nope.ts', '/nope.ts?rolldown-lazy=1'); \
         asked.then(() => (window.__nope = 'resolved'), (e) => (window.__nope = 'rejected: ' + e.message)); return true; })()",
    )
    .await;
    assert!(
        page.poll(
            "performance.getEntriesByType('resource').some((e) => e.name.includes('/@oj-start/hmr/lazy') && e.name.includes('nope'))",
            WAIT
        )
        .await
    );
    assert!(
        page.poll("String(window.__nope).startsWith('rejected: ')", WAIT)
            .await,
        "the import did not reject: {:?}",
        page.eval::<String>("String(window.__nope)").await
    );
    kept(&page, &dev).await;
    page.close().await;
}

/// Inside a lazily imported module: its own `import()`, a CommonJS
/// dependency through `import()`, an `import.meta.glob` member, a JSON file
/// imported with an attribute (`with { type: "json" }`), each edited
/// (where it is the app's) before the page first imports the module; then
/// its edits while requests for other modules are in flight. All in place.
/// (Not a stylesheet the module starts importing: an edit that adds one
/// right after another edit can lose the sheet's link, because the output
/// refresh of the first edit records the sheet as linked before the second
/// edit's hot update looks for sheets to link, and that is not this
/// test's to pin down.)
#[tokio::test(flavor = "multi_thread")]
async fn a_lazy_modules_own_imports_are_answered_as_they_are_now() {
    let Some(app) = lazy_app(
        "lazy-import-nested",
        "export const EVENTS = [\"a\", \"b\"];\n",
    ) else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    let inner = "src/lib/inner.ts";
    let member = "src/lib/members/one.ts";
    let note_of = |tag: &str, extra: &str| {
        format!(
            r#"{extra}const members = import.meta.glob("./members/*.ts");
const inner = await import("./inner");
const cjs = await import("legacy-cjs");
const one = (await members["./members/one.ts"]()) as {{ member: string }};
const data = await import("./nested.json", {{ with: {{ type: "json" }} }});
export const note = "{tag}|" + inner.inner + "|" + (cjs.default ?? cjs).badge("cjs") + "|" + one.member + "|" + data.default.k;
"#
        )
    };
    write(&root, inner, "export const inner = \"inner v1\";\n");
    write(&root, member, "export const member = \"member v1\";\n");
    write(&root, "src/lib/nested.json", "{ \"k\": \"json v1\" }\n");
    write(&root, NOTE, &note_of("n1", ""));
    let dev = Server::oj_dev(&root);
    let page = open_home(&dev).await;

    let before = hot_updates(&dev);
    write(&root, inner, "export const inner = \"inner v2\";\n");
    hot_updated(&dev, before).await;
    let before = hot_updates(&dev);
    write(&root, member, "export const member = \"member v2\";\n");
    hot_updated(&dev, before).await;
    let before = hot_updates(&dev);
    write(&root, "src/lib/nested.json", "{ \"k\": \"json v2\" }\n");
    hot_updated(&dev, before).await;
    notes_once_current(&page, &dev, "n1|inner v2|[CJS]|member v2|json v2").await;

    // Edits while other pages' requests keep the bundler compiling.
    let proxy = proxy_of(&dev, "src/routes/about.tsx?tsr-split=component");
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let asking = {
        let (stop, url) = (stop.clone(), dev.url(""));
        let port = url
            .rsplit(':')
            .next()
            .unwrap()
            .trim_end_matches('/')
            .to_string();
        std::thread::spawn(move || {
            let mut n = 0;
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                n += 1;
                let _ = std::process::Command::new("curl")
                    .args(["-s", "-o", "/dev/null", "-m", "5"])
                    .arg(format!(
                        "http://127.0.0.1:{port}{}",
                        lazy_url(&proxy, &format!("other-{n}"))
                    ))
                    .status();
            }
        })
    };
    for tag in ["n3", "n4", "n5"] {
        let before = hot_updates(&dev);
        write(&root, NOTE, &note_of(tag, ""));
        hot_updated(&dev, before).await;
        notes_once_current(
            &page,
            &dev,
            &format!("{tag}|inner v2|[CJS]|member v2|json v2"),
        )
        .await;
    }
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = asking.join();
    kept(&page, &dev).await;
    assert!(page.errors().is_empty(), "page errors: {:?}", page.errors());
    page.close().await;
}

/// A component in a lazily loaded route keeps its state when the route is
/// edited (Fast Refresh), and when it was first loaded after an edit.
#[tokio::test(flavor = "multi_thread")]
async fn a_lazily_loaded_component_keeps_its_state_across_an_edit() {
    let Some(app) = lazy_app(
        "lazy-import-refresh",
        "export const EVENTS = [\"a\", \"b\"];\n",
    ) else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    let counting = |tag: &str| {
        format!(
            r#"import {{ createFileRoute }} from "@tanstack/react-router";
import {{ useState }} from "react";

export const Route = createFileRoute("/about")({{
  component: AboutPage,
}});

function AboutPage() {{
  const [n, setN] = useState(0);
  return (
    <p>
      <span id="about">{tag}</span>
      <button id="about-count" onClick={{() => setN((v) => v + 1)}}>{{n}}</button>
    </p>
  );
}}
"#
        )
    };
    write(&root, ABOUT, &counting("r1"));
    let dev = Server::oj_dev(&root);
    let page = open_home(&dev).await;

    let before = hot_updates(&dev);
    write(&root, ABOUT, &counting("r2"));
    hot_updated(&dev, before).await;
    go_to_about(&page).await;
    shows(&page, &dev, "r2").await;
    page.eval::<bool>(
        "(document.querySelector('#about-count').click(), document.querySelector('#about-count').click(), true)",
    )
    .await;
    write(&root, ABOUT, &counting("r3"));
    shows(&page, &dev, "r3").await;
    assert_eq!(
        page.eval::<String>("document.querySelector('#about-count').textContent")
            .await,
        "2",
        "the component lost its state"
    );
    kept(&page, &dev).await;
    page.close().await;
}

/// `oj dev` on a port of the test's choosing, killed when dropped.
struct OnPort(std::process::Child);

impl OnPort {
    fn start(root: &Path, port: u16) -> OnPort {
        let child = std::process::Command::new(env!("CARGO_BIN_EXE_oj"))
            .arg("dev")
            .arg(root)
            .args(["--port", &port.to_string()])
            .current_dir(root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let server = OnPort(child);
        let deadline = std::time::Instant::now() + WAIT;
        loop {
            let up = std::process::Command::new("curl")
                .args(["-s", "-o", "/dev/null", "-m", "5", "-w", "%{http_code}"])
                .arg(format!("http://127.0.0.1:{port}/"))
                .output()
                .is_ok_and(|out| out.stdout == b"200");
            if up {
                return server;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "oj dev never answered on {port}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for OnPort {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A tab left open across a restart of the dev server runs a bundle the new
/// run never made. It reloads when its socket reconnects, as a page under
/// Vite does once the server it lost answers again, without waiting for
/// something of the page to fail.
#[tokio::test(flavor = "multi_thread")]
async fn a_tab_open_across_a_restart_reloads_when_the_server_is_back() {
    let Some(app) = lazy_app(
        "lazy-import-restart",
        "export const EVENTS = [\"a\", \"b\"];\n",
    ) else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    let port = start_app::free_port();
    let first = OnPort::start(&root, port);
    let page = Page::open(&format!("http://127.0.0.1:{port}/")).await;
    hot_ready(&page).await;
    drop(first);
    write(
        &root,
        NOTE,
        "export const note = \"note after the restart\";\n",
    );
    let _second = OnPort::start(&root, port);

    // A new document, with no import made by the old one.
    hot_ready(&page).await;
    load_note(&page).await;
    let shown = page
        .poll(
            &format!("{NOTE_SHOWN} === 'note after the restart'"),
            Duration::from_secs(40),
        )
        .await;
    assert!(
        shown,
        "the reloaded page shows {:?}; page errors: {:?}",
        page.eval::<String>(NOTE_SHOWN).await,
        page.errors()
    );
    page.close().await;
}

/// The same under a dev `base`, with two imports made together during one
/// hold: a module the held change does not reach and a lazy route reading a
/// held module the page runs. Both are answered during the hold.
#[tokio::test(flavor = "multi_thread")]
async fn two_imports_in_one_hold_under_a_base_are_each_answered_by_the_rule() {
    let Some(app) = lazy_app(
        "lazy-import-held-base",
        "export const EVENTS = [\"a\", \"b\"];\n",
    ) else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    let config = std::fs::read_to_string(root.join("vite.config.ts")).unwrap();
    let based = config.replacen(
        "export default defineConfig({",
        "export default defineConfig({\n  base: \"/app/\",",
        1,
    );
    assert_ne!(based, config, "the fixture's config moved");
    write(&root, "vite.config.ts", &based);
    write(&root, INDEX, INDEX_READS);
    let dev = Server::oj_dev_with(&root, GATE);
    let page = open_at(&dev, "/app/").await;
    watch_held_imports(&page).await;

    let before = taken_in(&dev);
    held(
        &dev,
        &root,
        &[(EVENT, "export const EVENTS = [\"a\", \"b\", \"c\"];\n")],
    )
    .await;
    took_in(&dev, before).await;
    page.eval::<bool>(
        "(document.querySelector('#load').click(), document.querySelector('#to-about').click(), true)",
    )
    .await;
    notes(&page, &dev, "note v1").await;
    assert!(
        page.poll(&format!("{SHOWN} === '2'"), Duration::from_secs(15))
            .await,
        "the lazy route did not load during the hold: {:?}\n{}",
        page.eval::<String>(SHOWN).await,
        dev.output()
    );
    assert!(
        page.eval::<bool>(&format!("({HELD_IMPORT})('/app/@oj-start/hmr/lazy_held_')"))
            .await,
        "the kept chunk was not taken under the base: {:?}",
        page.eval::<Vec<String>>("globalThis.__heldImports").await
    );
    flush(&dev);
    assert!(
        page.poll(&format!("{SHOWN} === '3'"), Duration::from_secs(40))
            .await,
        "the flush did not update the route: {:?}",
        page.eval::<String>(SHOWN).await
    );
    kept(&page, &dev).await;
    page.close().await;
}

/// A kept chunk the dev server no longer has (taken, or pushed out): the
/// page asks for the module again, to be answered once nothing is held,
/// and gets it.
#[tokio::test(flavor = "multi_thread")]
async fn a_kept_chunk_that_is_gone_is_asked_for_again() {
    let Some(app) = lazy_app(
        "lazy-import-held-gone",
        "export const EVENTS = [\"a\", \"b\"];\n",
    ) else {
        return;
    };
    if browser::skip() {
        return;
    }
    let dev = Server::oj_dev_with(&app.0, GATE);
    let page = open_home(&dev).await;
    let proxy = proxy_of(&dev, "src/lib/note.ts");
    // Another page's kept chunk, and one that never was.
    let (status, _) = dev.request(
        "GET",
        "/@oj-start/hmr/lazy_held_0.js?clientId=someone-else",
        &[],
    );
    assert_eq!(status, 404);
    let imported = page
        .eval::<String>(&format!(
            "(async () => {{ const rt = globalThis.__rolldown_runtime__; \
             await rt.ojLazyWhileHeld([], '/@oj-start/hmr/lazy_held_999.js?clientId=' + rt.clientId, {proxy:?}); \
             return rt.initModule('src/lib/note.ts').note; }})()"
        ))
        .await;
    assert_eq!(imported, "note v1");
    kept(&page, &dev).await;
    page.close().await;
}

/// The flush lands while the page is fetching the chunk kept for it: the
/// patch the flush released is applied first, and the chunk must leave what
/// that patch registered alone. A later edit to a module the held one
/// imports re-runs the held module on the page from its factory: it must be
/// the patch's, not the one from before the hold.
#[tokio::test(flavor = "multi_thread")]
async fn a_flush_during_the_fetch_of_a_kept_chunk_keeps_the_patchs_factories() {
    let Some(app) = lazy_app(
        "lazy-import-held-race",
        "import { BASE } from \"./base\";\nexport const EVENTS = [...BASE, \"b\"];\n",
    ) else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    let base = "src/lib/base.ts";
    write(&root, base, "export const BASE = [\"a\"];\n");
    write(&root, INDEX, INDEX_READS);
    let dev = Server::oj_dev_with(&root, GATE);
    let page = open_home(&dev).await;
    // The kept chunk reaches the page two seconds late; the page notes
    // which update it had applied when the chunk was asked for and when it
    // was evaluated.
    page.eval::<bool>(
        "(() => { const rt = globalThis.__rolldown_runtime__; const real = rt.ojImport.bind(rt); \
         globalThis.__kept = []; \
         rt.ojImport = (url) => { if (!url.includes('lazy_held_')) return real(url); \
           globalThis.__kept.push('asked at update ' + rt.hmr.lastSeq); \
           return new Promise((r) => setTimeout(r, 2000)).then(() => { \
             globalThis.__kept.push('fetched at update ' + rt.hmr.lastSeq); return real(url); }); }; \
         return true; })()",
    )
    .await;

    let before = taken_in(&dev);
    held(
        &dev,
        &root,
        &[(
            EVENT,
            "import { BASE } from \"./base\";\nexport const EVENTS = [...BASE, \"b\", \"c\"];\n",
        )],
    )
    .await;
    took_in(&dev, before).await;
    go_to_about(&page).await;
    // The page decided to take the kept chunk; the flush lands during its
    // fetch.
    assert!(
        page.poll("(globalThis.__kept ?? []).length === 1", WAIT)
            .await,
        "the page did not ask for the kept chunk"
    );
    flush(&dev);
    shows(&page, &dev, "3").await;
    // The premise: the flush's patch was applied before the chunk ran.
    assert_eq!(
        page.eval::<Vec<String>>("globalThis.__kept").await,
        ["asked at update 0", "fetched at update 1"],
        "the flush did not land during the fetch of the kept chunk"
    );

    // The held module is re-run for an edit of what it imports.
    change(
        &dev,
        &root,
        &[(base, "export const BASE = [\"a\", \"z\"];\n")],
    )
    .await;
    shows(&page, &dev, "4").await;
    kept(&page, &dev).await;
    page.close().await;
}

/// Pages keep importing modules while an edit that needs a new bundle is
/// made: the dev server's rebundle ends, with no moment when no page asks.
/// (It could not while each compile made the bundler rebuild its output;
/// none does now, and this passes with or without the lock that keeps a
/// compile out of a bundle run.)
#[test]
fn a_rebundle_ends_while_pages_keep_importing() {
    let Some(app) = lazy_app(
        "lazy-import-busy",
        "export const EVENTS = [\"a\", \"b\"];\n",
    ) else {
        return;
    };
    let dev = Server::oj_dev(&app.0);
    let proxy = proxy_of(&dev, "src/routes/about.tsx?tsr-split=component");
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let port = dev.port;
    let asking: Vec<_> = (0..4)
        .map(|thread| {
            let (stop, proxy) = (stop.clone(), proxy.clone());
            std::thread::spawn(move || {
                let mut n = 0;
                while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                    n += 1;
                    let _ = std::process::Command::new("curl")
                        .args(["-s", "-o", "/dev/null", "-m", "5"])
                        .arg(format!(
                            "http://127.0.0.1:{port}{}",
                            lazy_url(&proxy, &format!("page-{thread}-{n}"))
                        ))
                        .status();
                }
            })
        })
        .collect();
    std::thread::sleep(Duration::from_millis(500));

    // A new route: the client needs a new bundle for it.
    write(
        &app.0,
        "src/routes/extra.tsx",
        "import { createFileRoute } from \"@tanstack/react-router\";\n\nexport const Route = createFileRoute(\"/extra\")({\n  component: () => <p id=\"extra\">extra</p>,\n});\n",
    );
    let deadline = std::time::Instant::now() + WAIT;
    let rebuilt = loop {
        if dev.output().contains("oj start: rebuilt") {
            break true;
        }
        if std::time::Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    for thread in asking {
        let _ = thread.join();
    }
    assert!(
        rebuilt,
        "the rebundle did not end while pages kept importing\n{}",
        dev.output()
            .lines()
            .filter(|l| l.contains("oj start:") || l.contains("bundl"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// The bundle does not build at all (a module is broken, and a full build
/// was asked for: here a file the bundle does not know is added), so the
/// bundler compiles nothing. An import of the broken module still rejects
/// at once with that module's own error; one of another module waits for a
/// bundle that builds, and gets its module then.
#[tokio::test(flavor = "multi_thread")]
async fn an_import_of_the_module_a_failed_build_is_about_rejects_with_its_error() {
    let Some(app) = lazy_app(
        "lazy-import-no-build",
        "export const EVENTS = [\"a\", \"b\"];\n",
    ) else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    let dev = Server::oj_dev(&root);
    let page = open_home(&dev).await;
    let proxy = proxy_of(&dev, "src/lib/note.ts");
    let outcome = format!(
        "Promise.race([globalThis.__rolldown_runtime__.requestLazy('src/lib/note.ts', {proxy:?}).then(() => 'resolved', (e) => 'rejected: ' + e.message), new Promise((r) => setTimeout(r, 8000, 'pending'))])"
    );
    let rejected = |outcome: &str| outcome.starts_with("rejected: ") && outcome.contains("note.ts");

    write(&root, NOTE, "export const note = \"broken;\n");
    let deadline = tokio::time::Instant::now() + WAIT;
    while !dev.output().contains("client bundle failed") {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the broken edit never failed the bundle\n{}",
            dev.output()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // A file the bundle does not know is added: the dev server asks the
    // bundler for a full build, which fails on the broken module, and the
    // bundler compiles nothing from then on: an unrelated module's request
    // waits.
    write(
        &root,
        "src/lib/unrelated-new-file.ts",
        "export const fresh = 1;\n",
    );
    let about_proxy = proxy_of(&dev, "src/routes/about.tsx?tsr-split=component");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let waits = std::process::Command::new("curl")
            .args(["-s", "-o", "/dev/null", "-m", "1"])
            .arg(dev.url(&lazy_url(&about_proxy, "probe")))
            .status()
            // curl's exit code for a request that timed out.
            .is_ok_and(|status| status.code() == Some(28));
        if waits {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the bundler kept answering with the file broken\n{}",
            dev.output()
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let again = page.eval::<String>(&outcome).await;
    assert!(
        rejected(&again) && !again.contains("last full build failed"),
        "the import with no bundle that builds: {again}"
    );

    // The other module's import, made now, is answered once the bundle
    // builds again.
    go_to_about(&page).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(
        page.eval::<String>(SHOWN).await,
        "",
        "the route loaded with no bundle"
    );
    write(&root, NOTE, "export const note = \"note fixed\";\n");
    shows_despite_overlay(&page, &dev, "2").await;
    notes_once_current(&page, &dev, "note fixed").await;
    let said: Vec<String> = page
        .console()
        .into_iter()
        .chain(page.errors())
        .filter(|line| line.contains("last full build failed"))
        .collect();
    assert!(
        said.is_empty(),
        "an import failed for the bundle's state: {said:?}"
    );
    page.close().await;
}
