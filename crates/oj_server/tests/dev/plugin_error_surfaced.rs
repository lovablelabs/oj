// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A plugin whose transform or load throws fails the module in dev: the
//! request is a 500 naming the plugin, the location and the code frame, and
//! the raw source is never served in its place. Modules the plugin leaves
//! alone still serve. The build half (non-zero exit, no output carrying the
//! raw source) is not covered here.

use crate::{App, INDEX_HTML};

const PLUGINS: &str = r#"export default [{
    name: "bad-plugin",
    resolveId(id) { return id === "virtual:boom" ? "\0boom" : null; },
    load(id) { if (id === "\0boom") throw new Error("load exploded"); return null; },
    transform(code, id) {
      if (id.split("?")[0].endsWith("bad.js")) this.error({ message: "transform exploded", loc: { line: 2, column: 3 }, frame: "2 | export const C = 2;\n  |   ^\n" });
      return null;
    },
  }];
"#;

#[tokio::test(flavor = "multi_thread")]
async fn throwing_transform_and_load_fail_the_module_with_the_plugin_error() {
    let app = App::new(&[
        ("package.json", r#"{"name":"plugerr","version":"1.0.0"}"#),
        (
            "src/bad.js",
            "export const B = \"RAW_SOURCE_MARKER\";\nexport const C = 2;\n",
        ),
        (
            "src/virt-user.js",
            "import \"virtual:boom\";\nexport const V = 1;\n",
        ),
        ("src/fine.js", "export const F = 1;\n"),
        (
            "src/main.js",
            "import \"./bad.js\";\nimport \"./fine.js\";\n",
        ),
        ("index.html", INDEX_HTML),
        ("oj.plugins.mjs", PLUGINS),
    ]);
    let server = app.serve().await;

    let bad = server.get("/src/bad.js").await;
    assert_eq!(
        bad.status, 500,
        "throwing transform must 500:\n{}",
        bad.body
    );
    assert!(
        bad.body.contains("[plugin:bad-plugin] transform exploded"),
        "error names the plugin:\n{}",
        bad.body
    );
    assert!(
        bad.body.contains("bad.js:2:3"),
        "error carries the location:\n{}",
        bad.body
    );
    assert!(
        bad.body.contains("2 | export const C = 2;"),
        "error carries the code frame:\n{}",
        bad.body
    );
    assert!(
        !bad.body.contains("RAW_SOURCE_MARKER"),
        "raw source must not be served:\n{}",
        bad.body
    );

    // The importer compiles; the virtual module it imports fails on fetch.
    let vbody = server.text("/src/virt-user.js").await;
    let virt_url = regex::Regex::new(r#"from\s+"([^"]+)""#)
        .unwrap()
        .captures(&vbody)
        .or_else(|| {
            regex::Regex::new(r#"import\s+"([^"]+)""#)
                .unwrap()
                .captures(&vbody)
        })
        .map(|c| c[1].to_string())
        .unwrap_or_else(|| panic!("importer should reference the virtual module url:\n{vbody}"));
    let vmod = server.get(&virt_url).await;
    assert_eq!(vmod.status, 500, "throwing load must 500:\n{}", vmod.body);
    assert!(
        vmod.body.contains("[plugin:bad-plugin] load exploded"),
        "load error names the plugin:\n{}",
        vmod.body
    );

    let fine = server.get("/src/fine.js").await;
    assert_eq!(
        fine.status, 200,
        "unaffected modules still serve:\n{}",
        fine.body
    );
}
