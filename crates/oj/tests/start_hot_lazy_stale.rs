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
//! and the render threw into the error boundary. Vite loads such a module
//! fresh, in place. The page's runtime re-runs the chunk's out-of-date
//! modules on their patch factories once it has run, and React Refresh swaps
//! the route's component, with no reload; a lazily loaded module that is no
//! refresh boundary reloads the page instead. The browser half skips without
//! Chrome, unless `OJ_REQUIRE_BROWSER` is set.

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

if (import.meta.hot && typeof window !== "undefined") {
  import.meta.hot.on("vite:beforeFullReload", (payload) => {
    sessionStorage.setItem("full-reload", JSON.stringify(payload));
    const hmr = (globalThis as { __rolldown_runtime__?: { hmr?: { reloadReason?: string } } }).__rolldown_runtime__?.hmr;
    sessionStorage.setItem("reload-reason", hmr?.reloadReason ?? "");
  });
}

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
        <Link to="/contact" id="to-contact">contact</Link>
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

/// The client chunks the page has loaded so far.
async fn scripts(page: &Page) -> usize {
    page.eval::<usize>(
        "performance.getEntriesByType('resource').filter((e) => /\\/@oj-start\\/[^/]*-[^/]*\\.js$/.test(e.name)).length",
    )
    .await
}

/// The runtime has handled every update it heard without asking for a
/// reload, and `dropped` no longer runs on the page.
async fn taken_in_place(page: &Page, dropped: &str) {
    let rt = "globalThis.__rolldown_runtime__";
    assert!(
        page.eval::<bool>(&format!(
            "{rt}.hmr.queue.then(() => {rt}.hmr.reloadPending === false && {rt}.hmr.deferredReload === undefined && !{rt}.isExecuted({dropped:?}))"
        ))
        .await,
        "the page asked for a reload, or kept running {dropped}"
    );
}

/// The reload's payload, and the reason the runtime gave for it.
async fn reloaded(page: &Page, reason: &str) {
    let (payload, why) = page
        .eval::<(String, String)>(
            "[String(window.__mark) + ' ' + sessionStorage.getItem('full-reload'), sessionStorage.getItem('reload-reason') ?? '']",
        )
        .await;
    assert_eq!(
        payload, r#"undefined {"type":"full-reload","path":"*"}"#,
        "the page did not reload with Vite's full-reload payload"
    );
    assert!(
        why.contains(reason),
        "the page reloaded for another reason: {why}"
    );
}

/// The page is the document `open_home` opened: its `window` marker is
/// there and no `vite:beforeFullReload` listener ran.
async fn kept(page: &Page) {
    assert_eq!(
        page.eval::<String>("String(window.__mark) + ' ' + sessionStorage.getItem('full-reload')")
            .await,
        "kept null",
        "the page reloaded instead of loading the route in place; console: {:?}",
        page.console()
    );
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
    kept(&page).await;
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
    kept(&page).await;
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
    kept(&page).await;
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
    kept(&page).await;
    settled(&page).await;
    page.close().await;
}

/// The lazy route is preloaded from its hovered link after the update: the
/// click shows the new values, and the page is never reloaded.
#[tokio::test(flavor = "multi_thread")]
async fn a_lazy_route_preloaded_after_its_update_shows_it_in_place() {
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
    kept(&page).await;
    settled(&page).await;
    page.close().await;
}

/// The lazy route reading `imports` from `from`, rendering `shown`.
fn about_from(from: &str, (imports, shown): (&str, &str)) -> String {
    about(imports, shown).replace("\"../lib/event\"", &format!("\"{from}\""))
}

/// After `setup` (an empty source removes the file) and with the lazy route
/// as `before` (unless `setup` writes it), one edit to the shared module (`event`, at `event_path`) and
/// to the route, which then reads `after` from `from`: the first visit shows
/// `expect` in place, and the route's next edit is taken in place too.
#[allow(clippy::too_many_arguments)]
async fn in_place(
    name: &str,
    setup: &[(&str, &str)],
    before: (&str, &str),
    event_path: &str,
    event: &str,
    from: &str,
    after: (&str, &str),
    expect: &str,
    chunks: usize,
) {
    let Some(app) = lazy_app(name, "export const EVENTS = [\"a\", \"b\"];\n") else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    for (rel, source) in setup {
        if source.is_empty() {
            std::fs::remove_file(root.join(rel)).unwrap();
        } else {
            write(&root, rel, source);
        }
    }
    if !setup.iter().any(|(rel, _)| *rel == ABOUT) {
        write(&root, ABOUT, &about_from(from, before));
    }
    let dev = Server::oj_dev_with(&root, GATE);
    let page = open_home(&dev).await;

    let reads = about_from(from, after);
    change(&dev, &root, &[(event_path, event), (ABOUT, &reads)]).await;
    heard(&page, &dev, 1).await;
    let loaded = scripts(&page).await;
    page.eval::<bool>("(document.querySelector('#to-about').click(), true)")
        .await;
    shows(&page, &dev, expect).await;
    kept(&page).await;
    if chunks > 0 {
        assert!(
            scripts(&page).await - loaded >= chunks,
            "the route loaded fewer than {chunks} chunks"
        );
    }

    change(
        &dev,
        &root,
        &[(ABOUT, &reads.replace("id=\"about\">", "id=\"about\">v2 "))],
    )
    .await;
    shows(&page, &dev, &format!("v2 {expect}")).await;
    kept(&page).await;
    page.close().await;
}

const ADDED: &str =
    "EVENTS.length + \"|\" + FEATURED.map((f) => f.toUpperCase()).join(\",\") + \"|\" + COUNT";
const EVENT_ADDED: &str =
    "export const EVENTS = [\"a\", \"b\"];\nexport const FEATURED = [\"a\"];\nexport const COUNT = 3;\n";

/// The shared module also exports a component, so it accepts its own
/// updates: the export it gains reaches the route in place.
#[tokio::test(flavor = "multi_thread")]
async fn an_unloaded_lazy_route_reads_an_export_added_to_a_component_module() {
    let badge = "export function EventBadge() {\n  return <b>badge</b>;\n}\n";
    in_place(
        "lazy-stale-component",
        &[
            ("src/lib/event.ts", ""),
            (
                "src/lib/event.tsx",
                &format!("export const EVENTS = [\"a\", \"b\"];\n{badge}"),
            ),
        ],
        ("EVENTS, EventBadge", "<><EventBadge />{EVENTS.length}</>"),
        "src/lib/event.tsx",
        &format!("{EVENT_ADDED}{badge}"),
        "../lib/event",
        (
            "EVENTS, FEATURED, COUNT, EventBadge",
            &format!("<><EventBadge />{{{ADDED}}}</>"),
        ),
        "badge2|A|3",
        0,
    )
    .await;
}

/// A re-export barrel between the shared module and the route.
#[tokio::test(flavor = "multi_thread")]
async fn an_unloaded_lazy_route_reads_an_export_added_behind_a_barrel() {
    in_place(
        "lazy-stale-barrel",
        &[("src/lib/index.ts", "export * from \"./event\";\n")],
        ("EVENTS", "EVENTS.length"),
        EVENT,
        EVENT_ADDED,
        "../lib",
        ("EVENTS, FEATURED, COUNT", ADDED),
        "2|A|3",
        0,
    )
    .await;
}

/// The edit moves the route's import from the shared module to a barrel
/// nothing imported before: the shared module the chunk holds is no longer
/// imported directly, and the route still loads in place.
#[tokio::test(flavor = "multi_thread")]
async fn an_unloaded_lazy_route_switched_to_a_new_barrel_loads_in_place() {
    let direct = about("EVENTS", "EVENTS.length");
    in_place(
        "lazy-stale-new-barrel",
        &[
            ("src/lib/index.ts", "export * from \"./event\";\n"),
            (ABOUT, &direct),
        ],
        ("EVENTS", "EVENTS.length"),
        EVENT,
        EVENT_ADDED,
        "../lib",
        ("EVENTS, FEATURED, COUNT", ADDED),
        "2|A|3",
        0,
    )
    .await;
}

/// An export renamed in the shared module and in the route, whose component
/// reads its own `Route` (TanStack's split component imports it back from
/// the route file, which imports the component: an import cycle).
#[tokio::test(flavor = "multi_thread")]
async fn an_unloaded_lazy_route_reads_a_renamed_export() {
    in_place(
        "lazy-stale-renamed",
        &[],
        ("EVENTS", "EVENTS.length"),
        EVENT,
        "export const ALL_EVENTS = [\"a\", \"b\"];\n",
        "../lib/event",
        (
            "ALL_EVENTS",
            "Object.keys(Route.useParams()).length + \"renamed:\" + ALL_EVENTS.map((f) => f.toUpperCase()).join(\",\")",
        ),
        "0renamed:A,B",
        0,
    )
    .await;
}

/// The lazy route reading loader data through its own `Route`, the usual
/// TanStack pattern, edited with the module before the first visit: the
/// cycle between the route file and its split component does not reload the
/// page.
#[tokio::test(flavor = "multi_thread")]
async fn a_lazy_route_reading_its_own_route_loads_in_place() {
    let route = |names: &str, shown: &str| {
        about(names, shown)
            .replace(
                "  component: AboutPage,",
                "  loader: () => ({ count: EVENTS.length }),\n  component: AboutPage,",
            )
            .replace(
                "function AboutPage() {\n",
                "function AboutPage() {\n  const { count } = Route.useLoaderData();\n",
            )
    };
    let Some(app) = lazy_app(
        "lazy-stale-own-route",
        "export const EVENTS = [\"a\", \"b\"];\n",
    ) else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    write(&root, ABOUT, &route("EVENTS", "count"));
    let dev = Server::oj_dev_with(&root, GATE);
    let page = open_home(&dev).await;

    let reads = route(
        "EVENTS, FEATURED, COUNT",
        "count + \"|\" + FEATURED.map((f) => f.toUpperCase()).join(\",\") + \"|\" + COUNT",
    );
    change(&dev, &root, &[(EVENT, EVENT_ADDED), (ABOUT, &reads)]).await;
    heard(&page, &dev, 1).await;
    page.eval::<bool>("(document.querySelector('#to-about').click(), true)")
        .await;
    shows(&page, &dev, "2|A|3").await;
    kept(&page).await;

    change(
        &dev,
        &root,
        &[(ABOUT, &reads.replace("id=\"about\">", "id=\"about\">v2 "))],
    )
    .await;
    shows(&page, &dev, "v2 2|A|3").await;
    kept(&page).await;
    page.close().await;
}

/// A JSON module gains a key the route reads by named import.
#[tokio::test(flavor = "multi_thread")]
async fn an_unloaded_lazy_route_reads_a_key_added_to_json() {
    in_place(
        "lazy-stale-json",
        &[("src/lib/event.json", "{ \"events\": [\"a\", \"b\"] }\n")],
        ("events", "events.length"),
        "src/lib/event.json",
        "{ \"events\": [\"a\", \"b\"], \"featured\": [\"a\"] }\n",
        "../lib/event.json",
        (
            "events, featured",
            "events.length + \"|\" + featured.map((f) => f.toUpperCase()).join(\",\")",
        ),
        "2|A",
        0,
    )
    .await;
}

/// The shared module has run on the page through a module the root route
/// imports, which accepts no update: the patch re-runs it, and the lazy
/// route reads the re-run instance.
#[tokio::test(flavor = "multi_thread")]
async fn an_unloaded_lazy_route_reads_a_module_a_non_boundary_ran() {
    let root = ROOT
        .replace(
            "import { useEffect } from \"react\";",
            "import { useEffect } from \"react\";\nimport { title } from \"../lib/format\";",
        )
        .replace(
            "<Outlet />",
            "<span id=\"title\">{title()}</span>\n        <Outlet />",
        );
    in_place(
        "lazy-stale-non-boundary",
        &[
            ("src/lib/format.ts", "import { EVENTS } from \"./event\";\nexport const title = () => \"events:\" + EVENTS.length;\n"),
            ("src/routes/__root.tsx", &root),
        ],
        ("EVENTS", "EVENTS.length"),
        EVENT,
        EVENT_ADDED,
        "../lib/event",
        ("EVENTS, FEATURED, COUNT", ADDED),
        "2|A|3",
        0,
    )
    .await;
}

/// A second lazy route reading the shared module, which puts it in a chunk
/// of its own that the route's chunk imports.
const CONTACT: &str = r#"import { createFileRoute } from "@tanstack/react-router";
import { EVENTS } from "../lib/event";

export const Route = createFileRoute("/contact")({
  component: ContactPage,
});

function ContactPage() {
  return <p id="contact">{EVENTS.join(",")}</p>;
}
"#;

/// The shared module and the route are edited together; the module's chunk
/// registers before the route's, and both are brought up to date at once.
#[tokio::test(flavor = "multi_thread")]
async fn a_module_two_unloaded_lazy_routes_share_reaches_the_route_in_place() {
    in_place(
        "lazy-stale-shared-chunk",
        &[("src/routes/contact.tsx", CONTACT)],
        ("EVENTS", "EVENTS.length"),
        EVENT,
        EVENT_ADDED,
        "../lib/event",
        ("EVENTS, FEATURED, COUNT", ADDED),
        "2|A|3",
        2,
    )
    .await;
}

/// The module two lazy routes share is edited alone; the route, unchanged,
/// shows its new value in place.
#[tokio::test(flavor = "multi_thread")]
async fn a_module_two_unloaded_lazy_routes_share_edited_alone_loads_in_place() {
    let Some(app) = lazy_app(
        "lazy-stale-shared-alone",
        "export const EVENTS = [\"a\", \"b\"];\n",
    ) else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    write(&root, "src/routes/contact.tsx", CONTACT);
    let dev = Server::oj_dev_with(&root, GATE);
    let page = open_home(&dev).await;

    change(
        &dev,
        &root,
        &[(EVENT, "export const EVENTS = [\"a\", \"b\", \"c\"];\n")],
    )
    .await;
    heard(&page, &dev, 1).await;
    page.eval::<bool>("(document.querySelector('#to-about').click(), true)")
        .await;
    shows(&page, &dev, "3").await;
    kept(&page).await;
    page.close().await;
}

/// The edit makes the route stop importing the module its chunk holds: the
/// route loads in place, and the module's next edit does not reload the page.
#[tokio::test(flavor = "multi_thread")]
async fn a_module_the_route_stopped_importing_takes_its_next_edit_in_place() {
    let Some(app) = lazy_app(
        "lazy-stale-dropped",
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

    let plain =
        about("EVENTS", "\"plain\"").replace("import { EVENTS } from \"../lib/event\";\n", "");
    change(&dev, &root, &[(ABOUT, &plain)]).await;
    heard(&page, &dev, 1).await;
    page.eval::<bool>("(document.querySelector('#to-about').click(), true)")
        .await;
    shows(&page, &dev, "plain").await;
    kept(&page).await;

    change(
        &dev,
        &root,
        &[(EVENT, "export const EVENTS = [\"a\", \"b\", \"c\"];\n")],
    )
    .await;
    heard(&page, &dev, 2).await;
    taken_in_place(&page, EVENT).await;
    shows(&page, &dev, "plain").await;
    kept(&page).await;
    page.close().await;
}

/// The shared module's top-level code throws when it runs a second time
/// (a custom element defined twice): its re-run on the current code fails,
/// and the page reloads rather than keep a half-applied update.
#[tokio::test(flavor = "multi_thread")]
async fn a_module_that_throws_when_run_twice_reloads_its_page() {
    let define = "customElements.define(\"x-event\", class extends HTMLElement {});\n";
    let Some(app) = lazy_app(
        "lazy-stale-run-twice",
        &format!("{define}export const EVENTS = [\"a\", \"b\"];\n"),
    ) else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    let dev = Server::oj_dev_with(&root, GATE);
    let page = open_home(&dev).await;

    change(
        &dev,
        &root,
        &[(
            EVENT,
            &format!("{define}export const EVENTS = [\"a\", \"b\", \"c\"];\n"),
        )],
    )
    .await;
    heard(&page, &dev, 1).await;
    page.eval::<bool>("(document.querySelector('#to-about').click(), true)")
        .await;
    let shown = page.poll(&format!("{SHOWN} === '3'"), WAIT).await;
    assert!(
        shown,
        "the page never showed 3: {:?}",
        page.eval::<String>(SHOWN).await
    );
    reloaded(&page, "on its current code threw").await;
    page.close().await;
}

/// A route reading the shared module through `format.ts`, which `/contact`
/// shares, while `/blog` shares the module itself: three chunks, each
/// importing the next. The module is edited alone; the route shows its new
/// value in place.
#[tokio::test(flavor = "multi_thread")]
async fn a_route_reading_an_edited_module_through_a_shared_chunk_shows_it_in_place() {
    let Some(app) = lazy_app(
        "lazy-stale-chain",
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
        "src/lib/format.ts",
        "import { EVENTS } from \"./event\";\nexport const title = () => \"events:\" + EVENTS.length;\n",
    );
    write(
        &root,
        ABOUT,
        &about("title", "title()").replace("\"../lib/event\"", "\"../lib/format\""),
    );
    write(
        &root,
        "src/routes/contact.tsx",
        &CONTACT
            .replace(
                "import { EVENTS } from \"../lib/event\";",
                "import { title } from \"../lib/format\";",
            )
            .replace("EVENTS.join(\",\")", "title()"),
    );
    write(
        &root,
        "src/routes/blog.tsx",
        &CONTACT
            .replace("/contact", "/blog")
            .replace("ContactPage", "BlogPage"),
    );
    let dev = Server::oj_dev_with(&root, GATE);
    let page = open_home(&dev).await;

    change(
        &dev,
        &root,
        &[(EVENT, "export const EVENTS = [\"a\", \"b\", \"c\"];\n")],
    )
    .await;
    heard(&page, &dev, 1).await;
    let loaded = scripts(&page).await;
    page.eval::<bool>("(document.querySelector('#to-about').click(), true)")
        .await;
    shows(&page, &dev, "events:3").await;
    kept(&page).await;
    assert!(
        scripts(&page).await - loaded >= 3,
        "the route did not load through three chunks"
    );
    page.close().await;
}

/// The route stops rendering a component module (it accepts its own
/// updates): the route loads in place, the module leaves the page, and its
/// next edit does not reload the page.
#[tokio::test(flavor = "multi_thread")]
async fn a_component_module_the_route_stopped_importing_leaves_the_page() {
    let Some(app) = lazy_app(
        "lazy-stale-dropped-component",
        "export const EVENTS = [\"a\", \"b\"];\n",
    ) else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    let badge = |text: &str| format!("export function Badge() {{\n  return <b>{text}</b>;\n}}\n");
    write(&root, "src/lib/badge.tsx", &badge("badge"));
    write(
        &root,
        ABOUT,
        &about("Badge", "<Badge />").replace("\"../lib/event\"", "\"../lib/badge\""),
    );
    let dev = Server::oj_dev_with(&root, GATE);
    let page = open_home(&dev).await;

    let plain =
        about("EVENTS", "\"plain\"").replace("import { EVENTS } from \"../lib/event\";\n", "");
    change(&dev, &root, &[(ABOUT, &plain)]).await;
    heard(&page, &dev, 1).await;
    page.eval::<bool>("(document.querySelector('#to-about').click(), true)")
        .await;
    shows(&page, &dev, "plain").await;
    kept(&page).await;

    change(&dev, &root, &[("src/lib/badge.tsx", &badge("badge v2"))]).await;
    heard(&page, &dev, 2).await;
    taken_in_place(&page, "src/lib/badge.tsx").await;
    shows(&page, &dev, "plain").await;
    kept(&page).await;
    page.close().await;
}

/// The module two lazy routes share is edited with `/about`: after `/about`
/// loaded in place, `/contact`, whose chunk imports the shared chunk that has
/// already run, shows the new value in place too.
#[tokio::test(flavor = "multi_thread")]
async fn the_second_route_sharing_an_edited_module_shows_it_in_place() {
    let Some(app) = lazy_app(
        "lazy-stale-shared-second",
        "export const EVENTS = [\"a\", \"b\"];\n",
    ) else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    write(&root, "src/routes/contact.tsx", CONTACT);
    let dev = Server::oj_dev_with(&root, GATE);
    let page = open_home(&dev).await;

    let event = "export const EVENTS = [\"a\", \"b\", \"c\"];\nexport const FEATURED = [\"a\"];\nexport const COUNT = 3;\n";
    change(
        &dev,
        &root,
        &[
            (EVENT, event),
            (ABOUT, &about("EVENTS, FEATURED, COUNT", ADDED)),
        ],
    )
    .await;
    heard(&page, &dev, 1).await;
    page.eval::<bool>("(document.querySelector('#to-about').click(), true)")
        .await;
    shows(&page, &dev, "3|A|3").await;
    kept(&page).await;

    page.eval::<bool>("(document.querySelector('#to-contact').click(), true)")
        .await;
    let shown = page
        .poll(
            "document.querySelector('#contact')?.textContent === 'a,b,c'",
            WAIT,
        )
        .await;
    assert!(
        shown,
        "/contact never showed the new value: {:?}",
        page.eval::<String>("document.querySelector('#contact')?.textContent ?? ''")
            .await
    );
    kept(&page).await;
    page.close().await;
}

/// `/about` and `/contact` read the edited module through `format.ts`, and
/// the edit makes `/about` stop importing it: after `/about` loaded, the
/// shared chunk's copy of `format.ts` is stale, and `/contact`, whose chunk
/// imports that copy, shows the new value in place.
#[tokio::test(flavor = "multi_thread")]
async fn a_route_reading_a_module_another_route_dropped_shows_the_edit_in_place() {
    let Some(app) = lazy_app(
        "lazy-stale-dropped-shared",
        "export const EVENTS = [\"a\", \"b\"];\n",
    ) else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    let reads_title = |rel_route: &str| {
        CONTACT
            .replace("/contact", rel_route)
            .replace(
                "import { EVENTS } from \"../lib/event\";",
                "import { title } from \"../lib/format\";",
            )
            .replace("EVENTS.join(\",\")", "title()")
    };
    write(&root, INDEX, INDEX_READS);
    write(
        &root,
        "src/lib/format.ts",
        "import { EVENTS } from \"./event\";\nexport const title = () => \"events:\" + EVENTS.length;\n",
    );
    write(
        &root,
        ABOUT,
        &about("title", "title()").replace("\"../lib/event\"", "\"../lib/format\""),
    );
    write(&root, "src/routes/contact.tsx", &reads_title("/contact"));
    let dev = Server::oj_dev_with(&root, GATE);
    let page = open_home(&dev).await;

    let plain =
        about("EVENTS", "\"plain\"").replace("import { EVENTS } from \"../lib/event\";\n", "");
    change(
        &dev,
        &root,
        &[
            (EVENT, "export const EVENTS = [\"a\", \"b\", \"c\"];\n"),
            (ABOUT, &plain),
        ],
    )
    .await;
    heard(&page, &dev, 1).await;
    page.eval::<bool>("(document.querySelector('#to-about').click(), true)")
        .await;
    shows(&page, &dev, "plain").await;
    kept(&page).await;

    page.eval::<bool>("(document.querySelector('#to-contact').click(), true)")
        .await;
    let shown = page
        .poll(
            "document.querySelector('#contact')?.textContent === 'events:3'",
            WAIT,
        )
        .await;
    assert!(
        shown,
        "/contact never showed the new value: {:?}",
        page.eval::<String>("document.querySelector('#contact')?.textContent ?? ''")
            .await
    );
    kept(&page).await;
    page.close().await;
}

/// A module the home route imports lazily on a click and calls through its
/// namespace, which is no refresh boundary, edited before its first load:
/// its chunk cannot be brought up to date in place, so the page reloads at
/// its next navigation and the listeners hear Vite's payload.
#[tokio::test(flavor = "multi_thread")]
async fn a_lazily_loaded_module_no_refresh_can_swap_reloads_its_page() {
    let Some(app) = lazy_app(
        "lazy-stale-fallback",
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
        "src/lib/label.ts",
        "export const label = () => \"label v1\";\n",
    );
    write(
        &root,
        INDEX,
        r#"import { createFileRoute } from "@tanstack/react-router";
import { useState } from "react";

export const Route = createFileRoute("/")({
  component: HomePage,
});

function HomePage() {
  const [text, setText] = useState("none");
  return (
    <p id="home">
      <button id="load" onClick={() => import("../lib/label").then((m) => setText(m.label()))}>load</button>
      {text}
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
        &[(
            "src/lib/label.ts",
            "export const label = () => \"label v2\";\n",
        )],
    )
    .await;
    heard(&page, &dev, 1).await;
    page.eval::<bool>("(document.querySelector('#load').click(), true)")
        .await;
    page.wait_for(
        "document.querySelector('#home')?.textContent.includes('label')",
        WAIT,
    )
    .await;
    page.eval::<bool>("(document.querySelector('#to-about').click(), true)")
        .await;
    shows(&page, &dev, "2").await;
    reloaded(&page, "src/routes/index.tsx?tsr-split=component accepts it").await;
    page.close().await;
}
