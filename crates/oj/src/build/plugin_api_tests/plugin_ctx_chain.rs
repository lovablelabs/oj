// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use regex::Regex;

use super::{build_app, js_under, scratch, write_files};

/// A transform calling this.resolve("virtual:x") / this.load({ id }) on an id
/// a sibling plugin serves must get that plugin's answer: Vite runs the
/// container's resolveId/load chain inside ctx.resolve/ctx.load. Before, the
/// host went straight to the Rust resolver and the disk (null / throw). Ids no
/// plugin claims still resolve on disk. (The dev-server half of this stays in
/// e2e/plugin-ctx-chain.mjs.)
#[tokio::test]
async fn build_ctx_resolve_and_load_run_the_sibling_plugin_chain() {
    let root = scratch("ctx-chain-build");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"ctxchain","version":"1.0.0"}"#),
            ("src/a.js", "export const A = \"a\";\n"),
            ("src/main.js", "import \"./a.js\";\nwindow.__ok = 1;\n"),
            (
                "index.html",
                r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>"#,
            ),
            (
                "oj.plugins.mjs",
                r#"export default [
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
      // Side effects (not comments) so the markers survive the build's minifier.
      return code
        + "\nwindow.__RESOLVED = " + JSON.stringify(r ? r.id : null) + ";"
        + "\nwindow.__LOADED = " + JSON.stringify(!!(info && info.code.includes("from-virtual"))) + ";"
        + "\nwindow.__DISK = " + JSON.stringify(!!(disk && disk.id.endsWith("/src/a.js"))) + ";\n";
    },
  },
];
"#,
            ),
        ],
    );
    if let Err(err) = build_app(&root).await {
        panic!("build failed:\n{err:#}");
    }

    let js = js_under(&root.join("dist/assets"));
    // The NUL prefix may be re-emitted as `\0`, `\u0000` or a raw NUL.
    let resolved =
        Regex::new(r#"__RESOLVED\s*=\s*["'`](\\0|\\u0000|\x00)virtual:msg["'`]"#).unwrap();
    assert!(
        resolved.is_match(&js),
        "build: this.resolve reached the sibling plugin:\n{js}"
    );
    let loaded = Regex::new(r"__LOADED\s*=\s*(true|!0)").unwrap();
    assert!(
        loaded.is_match(&js),
        "build: this.load ran the sibling's load hook:\n{js}"
    );
    let disk = Regex::new(r"__DISK\s*=\s*(true|!0)").unwrap();
    assert!(
        disk.is_match(&js),
        "build: unclaimed ids still resolve on disk:\n{js}"
    );
}
