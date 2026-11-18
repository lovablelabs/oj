// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A lazy route an open page has not loaded yet, after a hot update changed
//! it, a module it imports, or re-ran a module it imports. The page's entry
//! still imports the route's chunk by the name it was built with, and the
//! dev server serves that name with the code from before the update, bound
//! to the chunks of that build. So the first visit rendered the route as it
//! was before the update (old values), and when the update had been skipped
//! (nothing it changed had run on the page) the route's next hot patch bound
//! to the module's old exports: an export added in the edit was `undefined`
//! and the render threw into the error boundary. Vite compiles every lazy
//! load for the current graph; the page now reloads itself onto the current
//! bundle when such a chunk loads, at once after a navigation, or at the
//! next navigation when a link's preload loaded it. A lazy route the updates
//! left alone loads in place. The browser half skips without Chrome, unless
//! `OJ_REQUIRE_BROWSER` is set.

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
import { useEffect } from "react";

export const Route = createRootRoute({
  head: () => ({ meta: [{ title: "lazy stale" }] }),
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
        <Link to="/about" id="to-about">about</Link>
        <Outlet />
        <Scripts />
      </body>
    </html>
  );
}
"#;

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
    write(root, "src/routeTree.ts", "export {};\n");
    write(root, "src/router.tsx", ROUTER);
    Some(app)
}

/// An editor's change of `files`, flushed once the gate holds them all.
async fn change(dev: &Server, root: &Path, files: &[(&str, &str)]) {
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
    let (status, body) = dev.request("POST", "/__hmr_flush", &[]);
    assert_eq!(status, 200, "flush: {body}");
}

/// Opens `/` and waits until it hydrated and its hot client registered.
async fn open_home(dev: &Server) -> Page {
    let page = Page::open(&dev.url("/")).await;
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

/// The page stays as it is (no further reload) for a while.
async fn settled(page: &Page) {
    page.eval::<bool>("(window.__mark = 'settled', true)").await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        page.eval::<String>("String(window.__mark)").await,
        "settled",
        "the page kept reloading"
    );
}

/// One edit gives the shared module an export and makes the lazy route read
/// it; the first visit shows the new value, and the route's next edit is
/// taken in place.
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
    page.eval::<bool>("(document.querySelector('#to-about').click(), true)")
        .await;
    shows(&page, &dev, "2|A|3").await;
    settled(&page).await;

    change(
        &dev,
        &root,
        &[(ABOUT, &reads.replace("id=\"about\">", "id=\"about\">v2 "))],
    )
    .await;
    shows(&page, &dev, "v2 2|A|3").await;
    assert_eq!(
        page.eval::<String>("String(window.__mark)").await,
        "settled",
        "the route's own edit reloaded the page instead of taking it in place"
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
    page.eval::<bool>("(document.querySelector('#to-about').click(), true)")
        .await;
    shows(&page, &dev, "names:a,b").await;
    settled(&page).await;

    change(
        &dev,
        &root,
        &[(ABOUT, &reads.replace("id=\"about\">", "id=\"about\">v2 "))],
    )
    .await;
    shows(&page, &dev, "v2 names:a,b").await;
    assert_eq!(
        page.eval::<String>("String(window.__mark)").await,
        "settled",
        "the route's own edit reloaded the page instead of taking it in place"
    );
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
    page.eval::<bool>("(document.querySelector('#to-about').click(), true)")
        .await;
    shows(&page, &dev, "2").await;
    assert_eq!(
        page.eval::<String>("String(window.__mark)").await,
        "kept",
        "a lazy route no update changed reloaded the page"
    );
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
    page.eval::<bool>("(document.querySelector('#to-about').click(), true)")
        .await;
    shows(&page, &dev, "3").await;
    settled(&page).await;
    page.close().await;
}

/// The lazy route is preloaded from its hovered link after the update: the
/// page stays as it is until the click, which lands on the route with the
/// new values.
#[tokio::test(flavor = "multi_thread")]
async fn a_lazy_route_preloaded_after_its_update_reloads_at_the_click() {
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
        "performance.getEntriesByType('resource').some((e) => /\\/about-[^/]*\\.js/.test(e.name))",
        WAIT,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        page.eval::<String>("location.pathname + ' ' + String(window.__mark)")
            .await,
        "/ kept",
        "the preload reloaded the page the user is on"
    );
    page.eval::<bool>("(document.querySelector('#to-about').click(), true)")
        .await;
    shows(&page, &dev, "2|A|3").await;
    settled(&page).await;
    page.close().await;
}
