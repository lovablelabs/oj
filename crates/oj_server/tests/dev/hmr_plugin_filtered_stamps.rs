// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A plugin's `hotUpdate` that returns modules (the common
//! `hotUpdate({ modules }) { return modules }`) steers the update, and the
//! update is still an HMR update: Vite's `handleHMRUpdate` (server/hmr.ts)
//! takes one `monotonicDateNow()` per changed file, hands it to the hook as
//! `timestamp`, and `updateModules` invalidates every returned module with
//! it (`moduleGraph.invalidateModule(mod, seen, timestamp, true)`), so the
//! chain up from them carries that stamp. The filtered path used to send the
//! update without stamping anything: the re-fetched boundary named its
//! edited dep by the bare url the browser already had, and the page kept the
//! old module.

use std::path::Path;
use std::time::Duration;

use regex::Regex;

use crate::browser::Page;
use crate::{App, INDEX_HTML};

const WAIT: Duration = Duration::from_secs(20);

const PLUGINS: &str = r#"import fs from "node:fs";
export default [
  {
    name: "returns-modules",
    hotUpdate({ file, modules, timestamp }) {
      if (!file.endsWith("label.js")) return;
      fs.writeFileSync(__MARKS__ + "/timestamp", String(timestamp));
      // More than it was handed: a filter of its own, not the default path
      // (returning `modules` as they came is; see hmr_plugin_passthrough).
      return [...modules, { url: "/src/other.js" }];
    },
  },
];
"#;

/// The `?t=` a module's import of `imported` carries ("" for a bare import).
fn stamp_of(code: &str, imported: &str) -> String {
    let re = Regex::new(&format!(r#""{}(?:\?t=(\d+))?""#, regex::escape(imported))).unwrap();
    re.captures(code)
        .map(|c| c.get(1).map_or("", |m| m.as_str()).to_string())
        .unwrap_or_else(|| panic!("no import of {imported} in:\n{code}"))
}

async fn mark(marks: &Path, name: &str) -> String {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        match std::fs::read_to_string(marks.join(name)) {
            Ok(v) if !v.is_empty() => return v,
            _ => {}
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the hook never wrote {name}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hot_update_returning_modules_stamps_the_chain_with_the_hooks_timestamp() {
    // Outside the app root so the watcher never sees the mark file.
    let marks_dir = tempfile::tempdir().unwrap();
    let marks = marks_dir.path().canonicalize().unwrap();
    let plugins = PLUGINS.replace(
        "__MARKS__",
        &serde_json::to_string(&marks.display().to_string()).unwrap(),
    );
    // main -> App (self-accepting) -> label (edited)
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"hmr-filtered","version":"1.0.0"}"#,
        ),
        ("src/label.js", "export const label = \"v1\";\n"),
        ("src/other.js", "export const other = \"o\";\n"),
        (
            "src/App.js",
            "import { label } from \"./label.js\";\nimport { other } from \"./other.js\";\n\
             export const text = label + other;\n\
             if (import.meta.hot) import.meta.hot.accept();\n",
        ),
        (
            "src/main.js",
            "import { text } from \"./App.js\";\nwindow.__text = text;\n",
        ),
        ("index.html", INDEX_HTML),
        ("oj.plugins.mjs", &plugins),
    ]);
    let server = app.serve().await;
    // A page load: every module served and cached once.
    for url in [
        "/src/main.js",
        "/src/App.js",
        "/src/label.js",
        "/src/other.js",
    ] {
        server.text(url).await;
    }
    assert_eq!(
        stamp_of(&server.text("/src/App.js").await, "/src/label.js"),
        "",
        "nothing is stamped before an edit"
    );

    app.write(&[("src/label.js", "export const label = \"v2\";\n")]);
    let timestamp = mark(&marks, "timestamp").await;

    // The boundary's re-fetch names the edited module under the update's
    // stamp, which is the one the hook was handed.
    let deadline = tokio::time::Instant::now() + WAIT;
    let stamp = loop {
        let stamp = stamp_of(&server.text("/src/App.js").await, "/src/label.js");
        if !stamp.is_empty() || tokio::time::Instant::now() >= deadline {
            break stamp;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(
        stamp, timestamp,
        "App.js names label.js under {stamp:?}; hotUpdate was handed {timestamp:?}"
    );
    assert_eq!(
        stamp_of(&server.text("/src/App.js").await, "/src/other.js"),
        timestamp,
        "the module the hook added is a module of the update too"
    );
    // And above the boundary, as for an update no plugin filtered.
    assert_eq!(
        stamp_of(&server.text("/src/main.js").await, "/src/App.js"),
        timestamp,
        "main.js names the boundary under the update's stamp"
    );
}

/// A plugin whose `transform` of `gen.js` reads `data.txt`, and whose
/// `hotUpdate` answers a change of `data.txt` with `gen.js`: the returned
/// module is not the changed file.
const GENERATED: &str = r#"import fs from "node:fs";
import path from "node:path";
let root;
export default [
  {
    name: "generated",
    configResolved(c) { root = c.root; },
    transform(code, id) {
      if (!id.endsWith("gen.js")) return;
      const data = fs.readFileSync(path.join(root, "data.txt"), "utf8").trim();
      return code.replace("__DATA__", data);
    },
    hotUpdate({ file }) {
      if (!file.endsWith("data.txt")) return;
      return [{ url: "/src/gen.js", id: path.join(root, "src/gen.js") }];
    },
  },
];
"#;

/// `main.js` accepts `gen.js` as a dep and shows what it exports.
const ACCEPTS_GEN: &str = "import { data } from \"./gen.js\";\ndocument.title = data;\n\
    if (import.meta.hot) import.meta.hot.accept(\"./gen.js\", (m) => { document.title = m.data; });\n";

fn generated_app() -> App {
    App::new(&[
        (
            "package.json",
            r#"{"name":"hmr-generated","version":"1.0.0"}"#,
        ),
        ("data.txt", "one\n"),
        ("src/gen.js", "export const data = \"__DATA__\";\n"),
        ("src/main.js", ACCEPTS_GEN),
        ("index.html", INDEX_HTML),
        ("oj.plugins.mjs", GENERATED),
    ])
}

/// Vite's `updateModules` invalidates every module of the update before it
/// decides anything (`moduleGraph.invalidateModule(mod, ...)`, a hard
/// invalidation: the transform result is dropped), so a module the hook
/// returned is transformed again on its next request. Its file did not
/// change, so nothing else would drop the compile oj had cached for it.
#[tokio::test(flavor = "multi_thread")]
async fn a_module_the_hook_returned_is_transformed_again() {
    let app = generated_app();
    let server = app.serve().await;
    server.text("/src/main.js").await;
    assert!(
        server.text("/src/gen.js").await.contains("\"one\""),
        "the transform reads data.txt"
    );

    app.write(&[("data.txt", "two\n")]);
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let code = server.text("/src/gen.js").await;
        if code.contains("\"two\"") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "gen.js was never transformed again:\n{code}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
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

/// The update's entry names the module the boundary accepts (Vite's
/// `acceptedVia`), so a boundary that took the returned module through
/// `hot.accept("./gen.js", cb)` has its callback run with the new module.
/// The filtered path named the boundary itself, which matched no callback.
#[tokio::test(flavor = "multi_thread")]
async fn a_filtered_update_reaches_a_boundary_that_accepts_the_module_as_a_dep() {
    if crate::browser::skip() {
        return;
    }
    let app = generated_app();
    let server = app.serve().await;
    let listening = server.listen().await;
    let page = Page::open(&format!("http://127.0.0.1:{}/", listening.addr.port())).await;
    page.wait_for(r#"document.title === "one""#, WAIT).await;
    assert!(page.eval::<bool>(RECORD_FRAMES).await);

    app.write(&[("data.txt", "two\n")]);
    let updated = page.poll(r#"document.title === "two""#, WAIT).await;
    let frames: serde_json::Value = page.eval("window.__frames ?? null").await;
    assert!(
        updated,
        "the dep callback never ran with the new module: {frames}"
    );
    assert!(
        page.eval::<bool>("window.__NOT_RELOADED === true").await,
        "the page was reloaded: {frames}"
    );
    let entry = frames
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["type"] == "update")
        .flat_map(|f| f["updates"].as_array().unwrap().clone())
        .find(|u| u["path"] == "/src/main.js")
        .unwrap_or_else(|| panic!("no update for the boundary: {frames}"));
    assert_eq!(
        entry["acceptedPath"], "/src/gen.js",
        "the entry names the accepted module: {entry}"
    );
    assert_eq!(page.errors(), Vec::<String>::new(), "page errors");
    page.close().await;
}
