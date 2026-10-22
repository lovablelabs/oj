// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! An object-form `resolveId` whose `filter.id` is a string glob (Vite's
//! pluginFilter: picomatch, `dot: true`) or a flagged RegExp claims the
//! imports it matches, even ones that exist on disk: Vite offers every import
//! to plugin resolveId before its own resolver, so oj's must step aside. An
//! exclude-only filter claims nothing, like a function-form hook: oj resolves
//! a file on disk itself and offers the plugin only what it cannot resolve.

use regex::Regex;

use crate::{App, INDEX_HTML};

const PLUGINS: &str = r#"export default [
  {
    name: "glob-filtered",
    resolveId: {
      filter: { id: "**/*.magic" },
      handler(id) { return "\0magic:" + id; },
    },
    load: {
      filter: { id: /^\0magic:/ },
      handler(id) { return "export default " + JSON.stringify("glob " + id.slice(7)) + ";\n"; },
    },
  },
  {
    name: "flag-filtered",
    resolveId: {
      filter: { id: /\.MAGIC2$/i },
      handler(id) { return "\0magic2:" + id; },
    },
    load: {
      filter: { id: /^\0magic2:/ },
      handler(id) { return "export default " + JSON.stringify("flag " + id.slice(8)) + ";\n"; },
    },
  },
  {
    name: "exclude-only",
    resolveId: {
      filter: { id: { exclude: /\.css$/ } },
      handler(id) { return id.endsWith(".magic3") ? "\0magic3:" + id : null; },
    },
    load: {
      filter: { id: /^\0magic3:/ },
      handler(id) { return "export default " + JSON.stringify("excl " + id.slice(8)) + ";\n"; },
    },
  },
];
"#;

#[tokio::test(flavor = "multi_thread")]
async fn glob_and_flagged_id_filters_claim_and_exclude_only_falls_back() {
    let app = App::new(&[
        ("package.json", r#"{"name":"filter-globs","version":"1.0.0"}"#),
        (
            "src/main.js",
            "import a from \"/src/thing.magic\";\nimport b from \"/src/other.magic2\";\nimport c from \"/src/third.magic3\";\nimport d from \"/src/missing.magic3\";\nwindow.__R = [a, b, c, d];\n",
        ),
        ("src/thing.magic", "export default \"disk\";\n"),
        ("src/other.magic2", "export default \"disk\";\n"),
        ("src/third.magic3", "export default \"disk\";\n"),
        ("index.html", INDEX_HTML),
        ("oj.plugins.mjs", PLUGINS),
    ]);
    let server = app.serve().await;
    let main = server.text("/src/main.js").await;
    let urls: Vec<String> = Regex::new(r#"from "([^"]+)""#)
        .unwrap()
        .captures_iter(&main)
        .map(|c| c[1].to_string())
        .collect();
    assert_eq!(urls.len(), 4, "four imports rewritten:\n{main}");
    let a = server.text(&urls[0]).await;
    assert!(
        a.contains("glob /src/thing.magic"),
        "the glob-filtered plugin resolved and loaded it ({}):\n{a}",
        urls[0]
    );
    let b = server.text(&urls[1]).await;
    assert!(
        b.contains("flag /src/other.magic2"),
        "the case-insensitive RegExp filter claimed it ({}):\n{b}",
        urls[1]
    );
    let c = server.text(&urls[2]).await;
    assert!(
        c.contains("\"disk\""),
        "an exclude-only filter leaves a file on disk to oj's resolver ({}):\n{c}",
        urls[2]
    );
    let d = server.text(&urls[3]).await;
    assert!(
        d.contains("excl /src/missing.magic3"),
        "an import oj cannot resolve still falls back to it ({}):\n{d}",
        urls[3]
    );
}
