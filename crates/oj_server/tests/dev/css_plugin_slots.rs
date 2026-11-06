// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A stylesheet's plugin transforms in dev run in Vite's slots around
//! vite:css and vite:css-post: "pre" plugins see the source, "normal" ones
//! the CSS vite:css makes of it (a CSS module's classes scoped), "post" ones
//! what vite:css-post serves: the CSS for a `?direct` request, the JS module
//! for an import or `?inline`. A hook's own `order` moves it between slots,
//! each request is a module of its own with its query in the id, and a
//! repeated request runs no plugin again. The
//! expected lines are what Vite 8.3.5's dev server records for this app.

use crate::{App, INDEX_HTML};

const PLUGINS: &str = r#"import { appendFileSync } from "node:fs";
const record = (name) => function (code, id) {
  if (!/\.(scss|css)(\?|$)/.test(id)) return null;
  const kind = code.includes("$c")
    ? "source"
    : /export (default|const)|^$/.test(code) || code.startsWith("import")
      ? "js"
      : /\.m[\s{]/.test(code) || !id.includes(".module.")
        ? "css"
        : "css-scoped";
  appendFileSync(__SEEN__, `${name} ${id.slice(id.lastIndexOf("/") + 1)} ${kind}\n`);
  return null;
};
const spy = (name, enforce) => ({ name, enforce, transform: record(name) });
const ordered = (name, enforce, order) => ({ name, enforce, transform: { order, handler: record(name) } });
export default [
  spy("post", "post"),
  spy("normal"),
  spy("pre", "pre"),
  ordered("normal-order-pre", undefined, "pre"),
  ordered("pre-order-post", "pre", "post"),
  ordered("post-order-pre", "post", "pre"),
];
"#;

#[tokio::test(flavor = "multi_thread")]
async fn stylesheet_transforms_run_in_vites_dev_slots() {
    // Outside the app root so the watcher never sees the record itself.
    let seen_dir = tempfile::tempdir().unwrap();
    let seen = seen_dir.path().join("seen.txt");
    let plugins = PLUGINS.replace(
        "__SEEN__",
        &serde_json::to_string(&seen.display().to_string()).unwrap(),
    );
    let app = App::new(&[
        ("package.json", r#"{"name":"csstx-slots","version":"1.0.0"}"#),
        ("src/a.scss", "$c: red;\n.a { color: $c; }\n"),
        ("src/b.scss", "$c: blue;\n.b { color: $c; }\n"),
        ("src/m.module.css", ".m { color: teal; }\n"),
        (
            "src/main.js",
            "import \"./a.scss\";\nimport b from \"./b.scss?inline\";\nimport m from \"./m.module.css\";\nconsole.log(b, m);\n",
        ),
        ("index.html", INDEX_HTML),
        ("oj.plugins.mjs", &plugins),
    ]);
    let server = app.serve().await;

    let wrapper = server.text("/src/a.scss").await;
    assert!(wrapper.contains("updateStyle"), "{wrapper}");
    let direct = server.text("/src/a.scss?direct").await;
    assert!(direct.contains(".a") && direct.contains("red"), "{direct}");
    let inline = server.text("/src/b.scss?inline").await;
    assert!(inline.starts_with("export default"), "{inline}");
    let module = server.text("/src/m.module.css").await;
    assert!(module.contains("\"m\":\"_m_"), "{module}");
    // Asked again, each module is served from its transform: no plugin
    // runs twice.
    for url in [
        "/src/a.scss",
        "/src/a.scss?direct",
        "/src/b.scss?inline",
        "/src/m.module.css",
    ] {
        server.text(url).await;
    }

    let seen = std::fs::read_to_string(&seen).expect("the plugins ran");
    let mut seen: Vec<&str> = seen.lines().collect();
    seen.sort_unstable();
    assert_eq!(
        seen,
        [
            "normal a.scss css",
            "normal a.scss?direct css",
            "normal b.scss?inline css",
            "normal m.module.css css-scoped",
            "normal-order-pre a.scss source",
            "normal-order-pre a.scss?direct source",
            "normal-order-pre b.scss?inline source",
            "normal-order-pre m.module.css css",
            "post a.scss js",
            "post a.scss?direct css",
            "post b.scss?inline js",
            "post m.module.css js",
            "post-order-pre a.scss source",
            "post-order-pre a.scss?direct source",
            "post-order-pre b.scss?inline source",
            "post-order-pre m.module.css css",
            "pre a.scss source",
            "pre a.scss?direct source",
            "pre b.scss?inline source",
            "pre m.module.css css",
            "pre-order-post a.scss js",
            "pre-order-post a.scss?direct css",
            "pre-order-post b.scss?inline js",
            "pre-order-post m.module.css js",
        ]
    );
}
