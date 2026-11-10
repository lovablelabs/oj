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

use crate::{App, INDEX_HTML};

const WAIT: Duration = Duration::from_secs(20);

const PLUGINS: &str = r#"import fs from "node:fs";
export default [
  {
    name: "returns-modules",
    hotUpdate({ file, modules, timestamp }) {
      if (!file.endsWith("label.js")) return;
      fs.writeFileSync(__MARKS__ + "/timestamp", String(timestamp));
      return modules;
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
        (
            "src/App.js",
            "import { label } from \"./label.js\";\nexport const text = label;\n\
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
    for url in ["/src/main.js", "/src/App.js", "/src/label.js"] {
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
    // And above the boundary, as for an update no plugin filtered.
    assert_eq!(
        stamp_of(&server.text("/src/main.js").await, "/src/App.js"),
        timestamp,
        "main.js names the boundary under the update's stamp"
    );
}
