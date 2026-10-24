// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The app's vite is imported the way Node's `import("vite")` finds it: the
//! `import` condition of its exports. Vite 5 and 6 map `require` to a CJS
//! proxy exporting little more than `defineConfig`, which `require.resolve`
//! picked: the host then saw no `version`, `resolveConfig` or `parseAst`. Here
//! a plugin reports the `this.meta.viteVersion` the host read from vite.

use crate::{App, INDEX_HTML};

const PLUGINS: &str = r#"export default [{
  name: "report-vite-version",
  transform(code, id) {
    return id.endsWith("/main.js") ? code.replace("__VITE__", this.meta.viteVersion) : null;
  },
}];
"#;

#[tokio::test(flavor = "multi_thread")]
async fn the_host_imports_vites_esm_entry_not_its_cjs_proxy() {
    let app = App::new(&[
        ("package.json", r#"{"name":"vite-esm","version":"1.0.0"}"#),
        (
            "node_modules/vite/package.json",
            r#"{"name":"vite","version":"5.99.0","type":"module","main":"./dist/node/index.js","exports":{".":{"import":{"types":"./dist/node/index.d.ts","default":"./dist/node/index.js"},"require":{"types":"./index.d.cts","default":"./index.cjs"}},"./package.json":"./package.json"}}"#,
        ),
        (
            "node_modules/vite/dist/node/index.js",
            "export const version = \"5.99.0-esm\";\nexport const defineConfig = (c) => c;\n",
        ),
        (
            "node_modules/vite/index.cjs",
            "module.exports = { defineConfig: (c) => c };\n",
        ),
        ("src/main.js", "window.__V = \"__VITE__\";\n"),
        ("index.html", INDEX_HTML),
        ("oj.plugins.mjs", PLUGINS),
    ]);
    let server = app.serve().await;
    let main = server.text("/src/main.js").await;
    assert!(
        main.contains("5.99.0-esm"),
        "the plugin saw vite's ESM build:\n{main}"
    );
}
