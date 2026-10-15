// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The build half of the plugin-error check: a plugin transform that calls
//! `this.error` fails the build with an error naming the plugin, and no
//! output carries the raw untransformed source. Bundling the raw source
//! instead would ship wrong code silently.

use super::{build_app, files_under, scratch, write_files};

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

#[tokio::test]
async fn throwing_transform_fails_the_build_naming_the_plugin() {
    let root = scratch("plugin-error-build");
    write_files(
        &root,
        &[
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
            (
                "index.html",
                r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>"#,
            ),
            ("oj.plugins.mjs", PLUGINS),
        ],
    );

    let err = build_app(&root)
        .await
        .expect_err("the build must fail when a plugin transform throws");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("bad-plugin"),
        "build error names the plugin:\n{msg}"
    );
    let dist = root.join("dist");
    let leaked: Vec<String> = files_under(&dist.join("assets"))
        .into_iter()
        .filter(|(_, code)| code.contains("RAW_SOURCE_MARKER"))
        .map(|(rel, _)| rel)
        .collect();
    assert!(
        !dist.join("index.html").exists() || leaked.is_empty(),
        "no output with the raw source: {leaked:?}"
    );
}
