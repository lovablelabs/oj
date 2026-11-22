// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A module an open page runs is edited to import, statically, a module the
//! page has never run: a component another (unvisited) route's chunk holds,
//! a package no module of the page imported yet, a file created by the same
//! edit. Under Vite's dev server the re-imported module's new `import` is a
//! request like any other, so the page shows the edit with its state kept,
//! and a route the page goes to meanwhile, which imports the same module,
//! renders.
//!
//! The last test is the case that failed: a dev server that booted on its
//! cached client bundle, then the editor's first edit, held by its gate.
//! The page, still on the cached bundle until the flush reloads it, went to
//! a route importing the module the edit made part of the entry, and the
//! route's module failed with `No factory registered for module`: the
//! bundler took the page for one that had loaded its newest output.
//! The browser tests skip without Chrome, unless `OJ_REQUIRE_BROWSER` is set.

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
const INDEX: &str = "src/routes/index.tsx";
const SHOWN: &str = "(document.querySelector('#home')?.textContent ?? '')";

const ROOT: &str = r#"import { createRootRoute, HeadContent, Link, Outlet, Scripts } from "@tanstack/react-router";
import { useEffect, useState } from "react";

export const Route = createRootRoute({
  head: () => ({ meta: [{ title: "new static imports" }] }),
  component: RootComponent,
});

function RootComponent() {
  const [clicks, setClicks] = useState(0);
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

/// The lazy route, the only importer of the shared button.
const ABOUT: &str = r#"import { createFileRoute } from "@tanstack/react-router";
import { Button } from "../components/button";

export const Route = createFileRoute("/about")({
  component: AboutPage,
});

function AboutPage() {
  return <p id="about"><Button label="about" /></p>;
}
"#;

const BUTTON: &str = r#"export function Button({ label }: { label: string }) {
  return <button className="shared">{"button:" + label}</button>;
}
"#;

/// The home route: `imports` above the route, `shown` inside its paragraph.
fn index(imports: &str, shown: &str) -> String {
    format!(
        r#"import {{ createFileRoute }} from "@tanstack/react-router";
{imports}
export const Route = createFileRoute("/")({{
  component: HomePage,
}});

function HomePage() {{
  return <p id="home">{shown}</p>;
}}
"#
    )
}

fn write(root: &Path, rel: &str, source: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, source).unwrap();
}

fn new_import_app(name: &str) -> Option<start_app::App> {
    let app = app(name)?;
    let root = &app.0;
    std::fs::remove_dir_all(root.join("src/routes")).unwrap();
    write(root, "src/routes/__root.tsx", ROOT);
    write(root, INDEX, &index("", "home"));
    write(root, "src/routes/about.tsx", ABOUT);
    write(root, "src/components/button.tsx", BUTTON);
    write(root, "src/routeTree.ts", "export {};\n");
    write(root, "src/router.tsx", ROUTER);
    Some(app)
}

/// An editor's change of `files`, flushed once the gate holds them all.
async fn change(dev: &Server, root: &Path, files: &[(&str, &str)]) {
    held(dev, root, files).await;
    flush(dev);
}

fn flush(dev: &Server) {
    let (status, body) = dev.request("POST", "/__hmr_flush", &[]);
    assert_eq!(status, 200, "flush: {body}");
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

const HOT_CLIENT: &str = "(() => { const bus = globalThis.__ojStartHmr; return !!(bus && bus.runtime && bus.send && bus.send({ type: 'oj:client', id: bus.runtime.clientId })); })()";

/// Opens `/`, hydrated with its hot client up, marked and with layout state
/// (a click on the counter) a reload would lose.
async fn open_home(dev: &Server) -> Page {
    let page = Page::open(&dev.url("/")).await;
    page.wait_for(
        &format!("window.__mark === undefined && self.__ojHydrated === true && {HOT_CLIENT}"),
        WAIT,
    )
    .await;
    page.eval::<bool>("(window.__mark = 'kept', document.querySelector('#count').click(), true)")
        .await;
    page.wait_for("document.querySelector('#count').textContent === '1'", WAIT)
        .await;
    page
}

fn server_lines(dev: &Server) -> String {
    dev.output()
        .lines()
        .filter(|l| l.contains("oj start:"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The home route shows `text` in the document the test opened, its layout
/// state kept, with no uncaught error and no render error on the way.
async fn shows_in_place(page: &Page, dev: &Server, text: &str) {
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
        server_lines(dev)
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
    for _ in 0..2 {
        assert_eq!(
            page.eval::<String>(
                "String(window.__mark) + ':' + document.querySelector('#count').textContent"
            )
            .await,
            "kept:1",
            "the page was reloaded\n{}",
            server_lines(dev)
        );
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }
}

/// The component only the unvisited lazy route imports, now imported by the
/// route the page shows.
#[tokio::test(flavor = "multi_thread")]
async fn a_shown_route_imports_a_component_only_an_unvisited_route_had() {
    let Some(app) = new_import_app("new-static-shared") else {
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
            INDEX,
            &index(
                "import { Button } from \"../components/button\";\n",
                "<Button label=\"home\" />",
            ),
        )],
    )
    .await;
    shows_in_place(&page, &dev, "button:home").await;
}

/// A file the edit creates, imported by the route the page shows.
#[tokio::test(flavor = "multi_thread")]
async fn a_shown_route_imports_a_file_created_with_its_edit() {
    let Some(app) = new_import_app("new-static-created") else {
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
        &[
            (
                "src/lib/regions.ts",
                "export const REGIONS = [\"north\", \"south\"];\n",
            ),
            (
                INDEX,
                &index(
                    "import { REGIONS } from \"../lib/regions\";\n",
                    "{\"regions:\" + REGIONS.join(\",\")}",
                ),
            ),
        ],
    )
    .await;
    shows_in_place(&page, &dev, "regions:north,south").await;
}

/// An installed package no module of the client imported, now imported by
/// the route the page shows.
#[tokio::test(flavor = "multi_thread")]
async fn a_shown_route_imports_a_package_the_client_never_imported() {
    let Some(app) = new_import_app("new-static-package") else {
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
            INDEX,
            &index(
                "import clsx from \"clsx\";\n",
                "{\"classes:\" + clsx(\"a\", { b: true })}",
            ),
        )],
    )
    .await;
    shows_in_place(&page, &dev, "classes:a b").await;
}

/// While the editor holds an edit that makes the shown route import the
/// shared component, the page goes to the route that always imported it:
/// the route renders, during the hold, and the held edit applies at the
/// flush.
#[tokio::test(flavor = "multi_thread")]
async fn a_route_visited_during_a_hold_runs_a_module_the_held_edit_newly_imports() {
    let Some(app) = new_import_app("new-static-held-visit") else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    let dev = Server::oj_dev_with(&root, GATE);
    let page = open_home(&dev).await;

    held(
        &dev,
        &root,
        &[(
            INDEX,
            &index(
                "import { Button } from \"../components/button\";\n",
                "<Button label=\"home\" />",
            ),
        )],
    )
    .await;
    // The hold's patch is made (and parked) before the click.
    let deadline = tokio::time::Instant::now() + WAIT;
    while !dev.output().contains("oj start: hot updated") {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the bundler never took the held edit in\n{}",
            dev.output()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    page.eval::<bool>("(document.querySelector('#to-about').click(), true)")
        .await;
    let shown = page
        .poll(
            "(document.querySelector('#about')?.textContent ?? '') === 'button:about'",
            Duration::from_secs(8),
        )
        .await;
    assert!(
        shown,
        "the route did not render during the hold: page errors: {:?}; console: {:?}\n{}",
        page.errors(),
        page.console(),
        server_lines(&dev)
    );
    flush(&dev);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(page.errors().is_empty(), "page errors: {:?}", page.errors());
    assert_eq!(
        page.eval::<String>(
            "String(window.__mark) + ':' + document.querySelector('#count').textContent + ':' + (document.querySelector('#about')?.textContent ?? '')"
        )
        .await,
        "kept:1:button:about",
        "{}",
        server_lines(&dev)
    );
}

/// The same app booted twice under the editor's gate: the second boot
/// restores the client bundle from the cache and runs no bundler until the
/// first edit.
fn restored(root: &Path) -> Server {
    {
        let first = Server::oj_dev_with(root, GATE);
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
    let dev = Server::oj_dev_with(root, GATE);
    let (status, _) = dev.request("GET", "/", &[]);
    assert_eq!(status, 200, "the second boot\n{}", dev.output());
    assert!(
        dev.output().contains("client bundle restored from cache"),
        "the second boot did not restore the bundle\n{}",
        dev.output()
    );
    dev
}

const ROOT_WITH_BUTTON: &str = r#"import { createRootRoute, HeadContent, Link, Outlet, Scripts } from "@tanstack/react-router";
import { useEffect, useState } from "react";
import { Button } from "../components/button";

export const Route = createRootRoute({
  head: () => ({ meta: [{ title: "new static imports" }] }),
  component: RootComponent,
});

function RootComponent() {
  const [clicks, setClicks] = useState(0);
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
        <span id="layout"><Button label="layout" /></span>
        <Outlet />
        <Scripts />
      </body>
    </html>
  );
}
"#;

/// A dev server that booted on its cached client bundle; the editor's first
/// edit, held by its gate, makes the layout import the shared component.
/// The page (still on the bundle from before the edit) goes to the route
/// that always imported the component: the route renders, and the layout
/// shows the edit after the flush.
#[tokio::test(flavor = "multi_thread")]
async fn a_route_visited_while_the_first_edit_after_a_restored_boot_is_held_renders() {
    let Some(app) = new_import_app("new-static-restored-held") else {
        return;
    };
    if browser::skip() {
        return;
    }
    let root = app.0.clone();
    let dev = restored(&root);
    let page = open_home(&dev).await;

    held(&dev, &root, &[("src/routes/__root.tsx", ROOT_WITH_BUTTON)]).await;
    // The bundler the edit started has built before the click.
    let deadline = tokio::time::Instant::now() + WAIT;
    while !(dev.output().contains("oj start: rebuilt")
        || dev.output().contains("oj start: hot updated"))
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the bundler never took the held edit in\n{}",
            dev.output()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    page.eval::<bool>("(document.querySelector('#to-about').click(), true)")
        .await;
    let shown = page
        .poll(
            "(document.querySelector('#about')?.textContent ?? '') === 'button:about'",
            Duration::from_secs(8),
        )
        .await;
    assert!(
        shown && page.errors().is_empty(),
        "the route did not render during the hold: shown {shown}; page errors: {:?}; console: {:?}\n{}",
        page.errors(),
        page.console(),
        server_lines(&dev)
    );
    flush(&dev);
    let after = page
        .poll(
            "(document.querySelector('#layout')?.textContent ?? '') === 'button:layout'",
            Duration::from_secs(40),
        )
        .await;
    assert!(
        after && page.errors().is_empty(),
        "the layout never showed the edit: page errors: {:?}; console: {:?}\n{}",
        page.errors(),
        page.console(),
        server_lines(&dev)
    );
}
