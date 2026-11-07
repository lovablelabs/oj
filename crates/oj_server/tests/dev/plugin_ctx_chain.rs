// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A transform calling `this.resolve("virtual:x")` / `this.load({ id })` on an
//! id a sibling plugin serves gets that plugin's answer (Vite runs the
//! container's resolveId/load chain inside ctx.resolve/ctx.load), and an id
//! no plugin claims still resolves on disk. This is the dev half; the build
//! half is crates/oj/src/build/plugin_api_tests/plugin_ctx_chain.rs. Before,
//! the host went straight to the Rust resolver and the disk: null / throw.

use crate::{App, INDEX_HTML};

const PLUGINS: &str = r#"export default [
  {
    name: "virt",
    resolveId(id) { return id === "virtual:msg" ? "\0virtual:msg" : null; },
    load(id) { return id === "\0virtual:msg" ? "export const MSG = 'from-virtual';" : null; },
  },
  {
    name: "consumer",
    async transform(code, id) {
      if (!id.split("?")[0].endsWith("/src/main.js")) return null;
      const r = await this.resolve("virtual:msg", id);
      const info = r ? await this.load({ id: r.id }) : null;
      const disk = await this.resolve("./a.js", id);
      return code
        + "\nwindow.__RESOLVED = " + JSON.stringify(r ? r.id : null) + ";"
        + "\nwindow.__LOADED = " + JSON.stringify(!!(info && info.code.includes("from-virtual"))) + ";"
        + "\nwindow.__DISK = " + JSON.stringify(!!(disk && disk.id.endsWith("/src/a.js"))) + ";\n";
    },
  },
];
"#;

#[tokio::test(flavor = "multi_thread")]
async fn ctx_resolve_and_load_run_the_sibling_plugin_chain_in_dev() {
    let app = App::new(&[
        ("package.json", r#"{"name":"ctxchain","version":"1.0.0"}"#),
        ("src/a.js", "export const A = \"a\";\n"),
        ("src/main.js", "import \"./a.js\";\nwindow.__ok = 1;\n"),
        ("index.html", INDEX_HTML),
        ("oj.plugins.mjs", PLUGINS),
    ]);
    let server = app.serve().await;
    let main = server.text("/src/main.js").await;

    // The NUL prefix may be re-emitted as `\0`, `\u0000` or a raw NUL.
    assert!(
        regex::Regex::new(r#"__RESOLVED = "(\\0|\\u0000|\x00)virtual:msg""#)
            .unwrap()
            .is_match(&main),
        "this.resolve reached the sibling plugin:\n{main}"
    );
    assert!(
        main.contains("__LOADED = true"),
        "this.load ran the sibling's load hook:\n{main}"
    );
    assert!(
        main.contains("__DISK = true"),
        "unclaimed ids still resolve on disk:\n{main}"
    );
}
