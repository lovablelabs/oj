// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Closing the dev server runs every plugin's buildEnd (no error) then
//! closeBundle, and booting it runs neither. As Vite's server.close closes
//! every environment's container, buildEnd runs in the client only and
//! closeBundle once per environment (client, ssr). The trigger (a
//! signal handled by `close_plugins_on_shutdown`, which then exits 130) ends
//! the process, so this runs `close_plugins`, the sequence that handler
//! runs. The build half is
//! crates/oj/src/build/plugin_api_tests/plugin_close_hooks.rs.

use std::path::Path;

use crate::{App, INDEX_HTML};

const PLUGINS: &str = r#"import fs from "node:fs";
const log = (line) => fs.appendFileSync(__EVENTS__, line + "\n");
export default [{
  name: "lifecycle",
  buildEnd(err) { log("buildEnd:" + (err ? err.message.split("\n")[0] : "ok")); },
  closeBundle() { log("closeBundle"); },
}];
"#;

fn events(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn dev_close_runs_build_end_then_close_bundle() {
    let marks = tempfile::tempdir().unwrap();
    let log = marks.path().join("events");
    let plugins = PLUGINS.replace(
        "__EVENTS__",
        &serde_json::to_string(&log.display().to_string()).unwrap(),
    );
    let app = App::new(&[
        ("package.json", r#"{"name":"closehooks","version":"1.0.0"}"#),
        ("src/main.js", "document.body.dataset.ok = \"1\";\n"),
        ("index.html", INDEX_HTML),
        ("oj.plugins.mjs", &plugins),
    ]);
    let server = app.serve().await;
    server.text("/").await;
    assert_eq!(
        events(&log),
        Vec::<String>::new(),
        "boot runs no close hook"
    );

    let host = server
        .built
        .plugin_host
        .clone()
        .expect("a plugin with close hooks keeps the host");
    // The sequence the signal handler runs; e2e/plugin-close-hooks.mjs
    // covers the signal reaching it.
    oj_server::close_plugins(&host).await;
    assert_eq!(
        events(&log),
        ["buildEnd:ok", "closeBundle", "closeBundle"],
        "dev close runs buildEnd then closeBundle"
    );
}
