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
    page.wait_for("self.__ojHydrated === true", WAIT).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    page.eval::<bool>("(window.__mark = 'kept', true)").await;
    page
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
    page.wait_for(
        "performance.getEntriesByType('resource').some((e) => e.name.includes('/@oj-start/hmr/lazy') && decodeURIComponent(e.name).includes('about.tsx'))",
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

    held(
        &dev,
        &root,
        &[
            (EVENT, "export const EVENTS = [\"a\", \"b\", \"c\"];\n"),
            (ABOUT, &about("EVENTS", "\"v2 \" + EVENTS.length")),
        ],
    )
    .await;
    // Past the bundler taking the change in.
    tokio::time::sleep(Duration::from_millis(1500)).await;
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

    held(&dev, &root, &[(INDEX, &index("home v2"))]).await;
    tokio::time::sleep(Duration::from_millis(1000)).await;
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
    load_note(&page).await;
    assert!(
        page.poll(
            &format!("{NOTE_SHOWN}.startsWith('failed: ')"),
            Duration::from_secs(40)
        )
        .await,
        "the import of a module that does not compile: {:?}\n{}",
        page.eval::<String>(NOTE_SHOWN).await,
        dev.output()
    );

    write(&root, NOTE, "export const note = \"note fixed\";\n");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    load_note(&page).await;
    notes(&page, &dev, "note fixed").await;
    kept(&page, &dev).await;
    page.close().await;
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
        while !first.output().contains("client bundle cached") {
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
    page.wait_for(
        "window.__mark === undefined && self.__ojHydrated === true",
        WAIT,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    load_note(&page).await;
    notes(&page, &dev, "note v2").await;

    // The bundler runs now: the next edit of the module, loaded, is a hot
    // update, and so is one of the lazy route before its first visit.
    page.eval::<bool>("(window.__mark = 'kept', true)").await;
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
    tokio::time::sleep(Duration::from_millis(1500)).await;
    load_note(&page).await;
    notes(&page, &dev, "note v2").await;
    assert!(page.errors().is_empty(), "page errors: {:?}", page.errors());
    page.close().await;
}
