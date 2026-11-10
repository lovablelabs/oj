// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `import.meta.hot.invalidate()`: the server answers with an update of the
//! module's importers that continues the update the module was touched by.
//! Vite's `vite:invalidate` handler (server/environment.ts) calls
//! `updateModules(this, file, [...mod.importers], mod.lastHMRTimestamp,
//! m.firstInvalidatedBy)`: the importers' update carries the module's own
//! `lastHMRTimestamp`, not a new time, so the importer re-fetched as
//! `?t=<that stamp>` imports the module under the url the page just loaded.

use std::time::Duration;

use crate::browser::Page;
use crate::{App, INDEX_HTML};

const WAIT: Duration = Duration::from_secs(20);

/// Self-accepting, and gives up on a new version whose value differs.
fn child(value: &str) -> String {
    format!(
        "export const value = \"{value}\";\n\
         if (import.meta.hot) import.meta.hot.accept((m) => {{\n\
         \x20 if (m && m.value !== \"{value}\") import.meta.hot.invalidate(\"value changed\");\n}});\n"
    )
}

const PARENT: &str = "import { value } from \"./Child.js\";\n\
    window.__parentRuns = (window.__parentRuns ?? 0) + 1;\ndocument.title = value;\n\
    if (import.meta.hot) import.meta.hot.accept();\n";

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

#[tokio::test(flavor = "multi_thread")]
async fn an_invalidation_updates_the_importers_under_the_modules_own_stamp() {
    if crate::browser::skip() {
        return;
    }
    // main -> Parent (self-accepting) -> Child (self-accepting, invalidates)
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"hmr-invalidate","version":"1.0.0"}"#,
        ),
        ("src/Child.js", &child("v1")),
        ("src/Parent.js", PARENT),
        ("src/main.js", "import \"./Parent.js\";\n"),
        ("index.html", INDEX_HTML),
    ]);
    let server = app.serve().await;
    let listening = server.listen().await;
    let page = Page::open(&format!("http://127.0.0.1:{}/", listening.addr.port())).await;
    page.wait_for(r#"document.title === "v1""#, WAIT).await;
    assert!(page.eval::<bool>(RECORD_FRAMES).await);

    app.write(&[("src/Child.js", &child("v2"))]);
    let updated = page.poll(r#"document.title === "v2""#, WAIT).await;
    let frames: serde_json::Value = page.eval("window.__frames ?? null").await;
    assert!(updated, "the importer never ran again: {frames}");
    assert!(
        page.eval::<bool>("window.__NOT_RELOADED === true").await,
        "the page was reloaded: {frames}"
    );
    assert_eq!(
        page.eval::<u32>("window.__parentRuns").await,
        2,
        "the importer ran once more"
    );

    let updates: Vec<serde_json::Value> = frames
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["type"] == "update")
        .flat_map(|f| f["updates"].as_array().unwrap().clone())
        .collect();
    let of = |path: &str| {
        updates
            .iter()
            .find(|u| u["path"] == path)
            .unwrap_or_else(|| panic!("no update of {path}: {frames}"))
    };
    let (edit, invalidated) = (of("/src/Child.js"), of("/src/Parent.js"));
    assert!(edit["timestamp"].is_u64(), "{edit}");
    assert_eq!(
        invalidated["timestamp"], edit["timestamp"],
        "the importers' update carries the module's lastHMRTimestamp: {frames}"
    );
    assert_eq!(
        invalidated["firstInvalidatedBy"], "/src/Child.js",
        "{invalidated}"
    );
    // The importer, served again, names the module under that same stamp:
    // the instance the page already loaded.
    let stamp = edit["timestamp"].as_u64().unwrap();
    let parent = server.text(&format!("/src/Parent.js?t={stamp}")).await;
    assert!(
        parent.contains(&format!("\"/src/Child.js?t={stamp}\"")),
        "Parent.js names Child.js under the update's stamp:\n{parent}"
    );
    assert_eq!(page.errors(), Vec::<String>::new(), "page errors");
    page.close().await;
}
