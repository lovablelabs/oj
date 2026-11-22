// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! What the dev server sends for an open page's `import()` when the page is
//! behind the bundle: its document is from before a rebuild whose reload
//! the editor's gate holds, or it lacks a hot change the gate holds (or one
//! its socket missed). The bundler leaves out of an answer the modules its
//! newest output evaluates as it loads, for a page that loaded that output.
//! A page behind it did not: a module the held edit made part of the entry
//! is one it never ran, and an answer without it fails on the page with
//! `No factory registered for module`. Under Vite the page's `import()` is
//! a request for the module, and each of its imports another, whatever the
//! page loaded before.
//!
//! The requests here are a page's own, made by hand: a runtime id that
//! starts with the bundler's tag (as every page of its bundles has), the
//! document's stamp (`g`) and the hot changes the page has (`h`).

#[path = "../../oj_server/tests/dev/browser.rs"]
#[allow(dead_code)]
mod browser;
#[allow(dead_code)]
mod start_app;

use std::path::Path;
use std::time::Duration;

use start_app::{app, Server, WAIT};

const GATE: &[(&str, &str)] = &[("OJ_HMR_GATE", "1"), ("OJ_HMR_FULL_RELOAD", "false")];
const ROOT: &str = "src/routes/__root.tsx";
/// What only the shared component's module holds.
const BUTTON_CODE: &str = "button:";

/// The layout; with `button`, importing the shared component.
fn root(button: bool) -> String {
    let (import, shown) = if button {
        (
            "import { Button } from \"../components/button\";\n",
            "<span id=\"layout\"><Button label=\"layout\" /></span>",
        )
    } else {
        ("", "")
    };
    format!(
        r#"import {{ createRootRoute, HeadContent, Link, Outlet, Scripts }} from "@tanstack/react-router";
import {{ useEffect }} from "react";
{import}
export const Route = createRootRoute({{
  head: () => ({{ meta: [{{ title: "pages behind" }}] }}),
  component: RootComponent,
}});

function RootComponent() {{
  useEffect(() => {{
    (self as {{ __ojHydrated?: boolean }}).__ojHydrated = true;
  }}, []);
  return (
    <html lang="en">
      <head>
        <HeadContent />
      </head>
      <body>
        <Link to="/about" id="to-about">about</Link>
        {shown}
        <Outlet />
        <Scripts />
      </body>
    </html>
  );
}}
"#
    )
}

const ROUTER: &str = r#"import { createRouter } from "@tanstack/react-router";
import { routeTree } from "./routeTree.gen";

export function getRouter() {
  return createRouter({ routeTree });
}
"#;

const INDEX: &str = r#"import { createFileRoute } from "@tanstack/react-router";

export const Route = createFileRoute("/")({
  component: () => <p id="home">home</p>,
});
"#;

/// The lazy route, the only importer of the shared component at first.
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

fn write(root: &Path, rel: &str, source: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, source).unwrap();
}

fn behind_app(name: &str) -> Option<start_app::App> {
    let app = app(name)?;
    let dir = &app.0;
    std::fs::remove_dir_all(dir.join("src/routes")).unwrap();
    write(dir, ROOT, &root(false));
    write(dir, "src/routes/index.tsx", INDEX);
    write(dir, "src/routes/about.tsx", ABOUT);
    write(dir, "src/components/button.tsx", BUTTON);
    write(dir, "src/routeTree.ts", "export {};\n");
    write(dir, "src/router.tsx", ROUTER);
    Some(app)
}

fn until(dev: &Server, what: &str, mut done: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + WAIT;
    while !done() {
        assert!(
            std::time::Instant::now() < deadline,
            "{what}\n{}",
            dev.output()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The stamp and the hot changes of a document rendered now, as its
/// live-reload script's url carries them.
fn document(dev: &Server) -> (String, u64) {
    let (status, html) = dev.request("GET", "/", &[]);
    assert_eq!(status, 200, "{html}");
    let query = html
        .split_once("/@oj-start/live-reload.js?")
        .expect("the document loads the live-reload client")
        .1;
    let query = &query[..query.find('"').unwrap()];
    let value = |key: &str| {
        query
            .split("&amp;")
            .find_map(|pair| pair.strip_prefix(key))
            .unwrap_or_else(|| panic!("no {key} in {query}"))
            .to_string()
    };
    (value("g="), value("h=").parse().unwrap())
}

/// The bundler's tag, as the client entry's first import names it.
fn engine_tag(dev: &Server) -> String {
    let (_, entry) = dev.request("GET", "/@oj-start/client-entry.js", &[]);
    entry
        .split_once("/@oj-start/engine.js?t=")
        .expect("the client entry imports its engine's tag")
        .1
        .chars()
        .take_while(char::is_ascii_hexdigit)
        .collect()
}

/// The proxy id the entry asks the dev server for, for the lazy route's
/// component.
fn about_proxy(dev: &Server) -> String {
    let module = "src/routes/about.tsx?tsr-split=component";
    let (_, entry) = dev.request("GET", "/@oj-start/client-entry.js", &[]);
    let end = entry
        .find(&format!("{module}?rolldown-lazy=1\""))
        .unwrap_or_else(|| panic!("the entry asks for no {module}"))
        + module.len()
        + "?rolldown-lazy=1".len();
    let start = entry[..end].rfind('"').unwrap() + 1;
    entry[start..end].to_string()
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

/// The code the page `client` would run for its `import()` of the lazy
/// route, given the document stamp and the hot changes it names: the
/// answer, or the chunk the answer tells the page to take during a hold.
fn route_code(dev: &Server, proxy: &str, client: &str, stamp: &str, has: u64) -> String {
    let url = format!(
        "/@oj-start/hmr/lazy?id={}&clientId={client}&n=1&g={stamp}&h={has}",
        encoded(proxy)
    );
    let (status, body) = dev.request("GET", &url, &[]);
    assert_eq!(status, 200, "{url}: {body}");
    let Some(at) = body.find("ojLazyWhileHeld(") else {
        return body;
    };
    // `ojLazyWhileHeld(<held ids>, "<chunk url>", <proxy>)`.
    let args = &body[at..];
    let from = args
        .find("], \"")
        .or_else(|| args.find("],\""))
        .expect("a kept chunk's url");
    let rest = &args[from..];
    let start = rest.find('"').unwrap() + 1;
    let end = start + rest[start..].find('"').unwrap();
    let (status, chunk) = dev.request("GET", &rest[start..end], &[]);
    assert_eq!(status, 200, "{}: {chunk}", &rest[start..end]);
    chunk
}

/// Asks, each time as a page the bundler has not heard of, until the answer
/// for a page that loaded the newest output leaves the component out: the
/// bundler's record of what that output evaluates has the held edit.
fn until_the_newest_output_has_the_button(dev: &Server, proxy: &str, tag: &str) {
    let mut n = 0;
    until(
        dev,
        "the bundler never took the component for part of the entry",
        || {
            n += 1;
            let (stamp, has) = document(dev);
            !route_code(dev, proxy, &format!("{tag}.current{n}"), &stamp, has).contains(BUTTON_CODE)
        },
    );
}

/// The editor's gate holds an edit that makes the layout import the shared
/// component, with no page open: the edit is a rebuild, its reload held. A
/// page whose document is from before it asks for the route that imports
/// the component and gets the component too; a page that loaded the new
/// bundle does not need it again.
#[test]
fn a_page_owed_a_held_reload_is_sent_what_the_rebuild_made_part_of_the_entry() {
    let Some(app) = behind_app("behind-held-reload") else {
        return;
    };
    let dir = app.0.clone();
    let dev = Server::oj_dev_with(&dir, GATE);
    let (before, has) = document(&dev);
    let tag = engine_tag(&dev);
    let proxy = about_proxy(&dev);

    write(&dir, ROOT, &root(true));
    until(&dev, "the held edit never rebuilt the bundle", || {
        dev.output().contains("reload held for the editor's flush")
    });
    until_the_newest_output_has_the_button(&dev, &proxy, &tag);

    let code = route_code(&dev, &proxy, &format!("{tag}.before"), &before, has);
    assert!(
        code.contains(BUTTON_CODE),
        "the page from before the rebuild got the route without the component:\n{code}"
    );
}

/// The same with a page open, so the edit is a hot update, parked by the
/// gate: nothing reloads, and the pages' documents stay current. A page
/// that lacks the parked change (it names the hot changes it has) and is
/// not known to the bundler (its socket is down, or its proxy passes none)
/// gets the component with the route.
#[tokio::test(flavor = "multi_thread")]
async fn a_page_behind_a_held_hot_change_is_sent_what_the_change_made_part_of_the_entry() {
    let Some(app) = behind_app("behind-held-hot-change") else {
        return;
    };
    if browser::skip() {
        return;
    }
    let dir = app.0.clone();
    let dev = Server::oj_dev_with(&dir, GATE);
    let (stamp, before) = document(&dev);
    let tag = engine_tag(&dev);
    let proxy = about_proxy(&dev);
    // An open page: with one, the edit goes out as a hot update.
    let page = browser::Page::open(&dev.url("/")).await;
    page.wait_for(
        "self.__ojHydrated === true && (() => { const bus = globalThis.__ojStartHmr; return !!(bus && bus.runtime && bus.send && bus.send({ type: 'oj:client', id: bus.runtime.clientId })); })()",
        WAIT,
    )
    .await;

    write(&dir, ROOT, &root(true));
    until(&dev, "the held edit never made a hot update", || {
        dev.output().contains("oj start: hot updated")
    });
    let (after_stamp, after) = document(&dev);
    assert_eq!(after_stamp, stamp, "a hot update owes the pages no reload");
    assert!(
        after > before,
        "the hot change is counted: {before} -> {after}"
    );
    until_the_newest_output_has_the_button(&dev, &proxy, &tag);

    let code = route_code(&dev, &proxy, &format!("{tag}.behind"), &stamp, before);
    assert!(
        code.contains(BUTTON_CODE),
        "the page behind the held change got the route without the component:\n{code}"
    );
    // The open page's own request names the changes it has.
    let asked = page
        .eval::<String>("globalThis.__rolldown_runtime__.lazyChunkUrl('x')")
        .await;
    assert!(
        asked.contains(&format!("&h={before}")),
        "the page's request names its hot changes: {asked}"
    );
    page.close().await;
}
