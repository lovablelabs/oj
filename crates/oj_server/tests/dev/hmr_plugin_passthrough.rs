// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A plugin's `hotUpdate` that returns the modules it was handed
//! (`hotUpdate({ modules }) { return modules }`, the commonest shape) changes
//! nothing: Vite's `updateModules` runs on the same modules as with no hook.
//! oj took any returned array for a filter of its own and left the default
//! decision out, which is also where it decides what Vite's graph holds a
//! module for and oj's does not: a `<link>` stylesheet (a `css-update` that
//! swaps the link; the filtered path sent a `js-update` no page handles) and
//! an html page (Vite's `hmr()`: "html file cannot be hot updated", a
//! `full-reload` whatever the hooks returned; oj said "suppressed by plugin").

use std::time::Duration;

use crate::browser::Page;
use crate::App;

const WAIT: Duration = Duration::from_secs(20);

const RETURNS_MODULES: &str =
    r#"export default [{ name: "passthrough", hotUpdate({ modules }) { return modules; } }];"#;
const RETURNS_NONE: &str = r#"export default [{ name: "none", hotUpdate() { return []; } }];"#;

fn index(heading: &str) -> String {
    format!(
        r#"<!doctype html><html><head><title>t</title><link rel="stylesheet" href="/style.css"></head><body><h1>{heading}</h1><script type="module" src="/src/main.js"></script></body></html>"#
    )
}

/// A second socket in the page records every frame the server sends.
const RECORD_FRAMES: &str = r#"(async () => {
  window.__frames = [];
  const client = await (await fetch("/@oj/client.js")).text();
  const token = /const wsToken = "([0-9a-f]+)";/.exec(client)[1];
  const ws = new WebSocket("ws://" + location.host + "/__ws?token=" + token);
  ws.onmessage = (e) => window.__frames.push(JSON.parse(e.data));
  await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
  window.__NOT_RELOADED = true;
  return true;
})()"#;

fn app(plugins: &str) -> App {
    App::new(&[
        (
            "package.json",
            r#"{"name":"hmr-passthrough","version":"1.0.0"}"#,
        ),
        ("style.css", "h1 { color: rgb(1, 2, 3); }\n"),
        ("src/main.js", "window.__ok = 1;\n"),
        ("index.html", &index("hi")),
        ("oj.plugins.mjs", plugins),
    ])
}

/// A page of the app at `port`, recording the frames it is sent.
async fn opened(port: u16) -> Page {
    let page = Page::open(&format!("http://127.0.0.1:{port}/")).await;
    page.wait_for("window.__ok === 1", WAIT).await;
    assert!(page.eval::<bool>(RECORD_FRAMES).await);
    page
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hook_returning_its_modules_keeps_the_link_stylesheet_update_and_the_html_reload() {
    if crate::browser::skip() {
        return;
    }
    let app = app(RETURNS_MODULES);
    let server = app.serve().await;
    // Held for the test: dropping it stops the listener.
    let listening = server.listen().await;
    let page = opened(listening.addr.port()).await;

    // The linked sheet: a css-update, applied with no reload.
    app.write(&[("style.css", "h1 { color: rgb(4, 5, 6); }\n")]);
    let swapped = page
        .poll(
            r#"getComputedStyle(document.querySelector("h1")).color === "rgb(4, 5, 6)""#,
            WAIT,
        )
        .await;
    let frames: serde_json::Value = page.eval("window.__frames ?? null").await;
    assert!(swapped, "the linked sheet never updated: {frames}");
    assert!(
        page.eval::<bool>("window.__NOT_RELOADED === true").await,
        "the page was reloaded: {frames}"
    );
    let entries: Vec<serde_json::Value> = frames
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["type"] == "update")
        .flat_map(|f| f["updates"].as_array().unwrap().clone())
        .collect();
    assert!(
        entries
            .iter()
            .any(|u| u["type"] == "css-update" && u["path"] == "/style.css"),
        "no css-update for the sheet: {frames}"
    );
    assert!(
        !entries.iter().any(|u| u["type"] == "js-update"),
        "a js-update for a linked sheet: {frames}"
    );

    // The page: reloaded onto the edit.
    app.write(&[("index.html", &index("hello"))]);
    page.wait_for(
        r#"document.querySelector("h1")?.textContent === "hello""#,
        WAIT,
    )
    .await;
    page.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hook_returning_no_modules_suppresses_a_module_update_but_not_an_html_reload() {
    if crate::browser::skip() {
        return;
    }
    let app = app(RETURNS_NONE);
    let server = app.serve().await;
    // Held for the test: dropping it stops the listener.
    let listening = server.listen().await;
    let page = opened(listening.addr.port()).await;

    // A module the page loaded: the hook took every module away, so nothing
    // is sent (Vite's "no modules matched").
    app.write(&[("src/main.js", "window.__ok = 1;\nwindow.__edited = 1;\n")]);
    tokio::time::sleep(Duration::from_secs(2)).await;
    let frames: serde_json::Value = page.eval("window.__frames ?? null").await;
    assert!(
        !frames
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["type"] == "update" || f["type"] == "full-reload"),
        "the suppressed edit reached the page: {frames}"
    );

    // An html page reloads all the same.
    app.write(&[("index.html", &index("hello"))]);
    page.wait_for(
        r#"document.querySelector("h1")?.textContent === "hello""#,
        WAIT,
    )
    .await;
    page.close().await;
}
