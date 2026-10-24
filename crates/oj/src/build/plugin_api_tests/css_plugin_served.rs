// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use super::{build_app, files_under, scratch, write_files};

fn built_css(root: &std::path::Path) -> String {
    files_under(&root.join("dist/assets"))
        .into_iter()
        .filter(|(rel, _)| rel.ends_with(".css"))
        .map(|(_, code)| code)
        .collect::<Vec<_>>()
        .join("\n")
}

/// A stylesheet a plugin's `load` serves (a virtual `.css` or `.scss` id) is
/// compiled like a file, as Vite runs vite:css on whatever load returned:
/// its rules reach the output and every transform slot sees it once, "pre"
/// as the served source.
#[tokio::test]
async fn a_plugin_served_stylesheet_is_compiled_and_shipped() {
    let root = scratch("css-plugin-served");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"css-served","version":"1.0.0"}"#),
            (
                "src/entry.js",
                "import \"virtual:theme.css\";\nimport \"virtual:tokens.scss\";\n",
            ),
            (
                "index.html",
                r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/entry.js"></script></body></html>"#,
            ),
            (
                "oj.plugins.mjs",
                r#"import { appendFileSync } from "node:fs";
const record = (name) => (code, id) => {
  if (!id.startsWith("\0virtual:")) return null;
  appendFileSync(new URL("./seen.txt", import.meta.url), `${name} ${id.slice(9)} ${code.includes("$c") ? "source" : "other"}\n`);
  return null;
};
export default [
  {
    name: "virtual-css",
    resolveId(id) { return id.startsWith("virtual:") ? "\0" + id : null; },
    load(id) {
      if (id === "\0virtual:theme.css") return ".from-plugin { color: rebeccapurple; }\n";
      if (id === "\0virtual:tokens.scss") return "$c: teal;\n.tokens { color: $c; }\n";
      return null;
    },
  },
  { name: "spy-pre", enforce: "pre", transform: record("pre") },
  { name: "spy-post", enforce: "post", transform: record("post") },
];
"#,
            ),
        ],
    );
    build_app(&root).await.expect("the app builds");

    let css = built_css(&root);
    assert!(css.contains(".from-plugin{color:#639}"), "{css}");
    assert!(
        css.contains(".tokens{color:teal}"),
        "the served SCSS compiled:\n{css}"
    );
    let seen = std::fs::read_to_string(root.join("seen.txt")).expect("the spies ran");
    let mut seen: Vec<&str> = seen.lines().collect();
    seen.sort_unstable();
    assert_eq!(
        seen,
        [
            "post theme.css other",
            "post tokens.scss other",
            "pre theme.css other",
            "pre tokens.scss source",
        ]
    );
}
