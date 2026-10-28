// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A plugin's `this.resolve` and `this.load` resolve with their own
//! environment's resolve options, as Vite builds its resolve plugin per
//! environment (`applyToEnvironment`, plugins/resolve.ts): the ssr environment
//! never takes a package's `browser` field. oj's three paths are covered: an
//! import of the transforming module (pre-resolved before the hook runs), a
//! specifier it does not import (resolved on demand), and the `importedIds`
//! of a module `this.load` reads from disk. Before, all three answered with
//! the client's options in SSR.

use crate::{App, INDEX_HTML};

const PLUGINS: &str = r#"export default [
  {
    name: "reports-resolve",
    async transform(code, id) {
      if (!id.split("?")[0].endsWith("/src/main.js")) return null;
      const short = (p) => (p ? p.split("/").slice(-2).join("/") : null);
      const name = async (spec) => short((await this.resolve(spec, id))?.id);
      const dep = id.split("?")[0].replace(/main\.js$/, "dep.js");
      const loaded = await this.load({ id: dep });
      const seen = {
        imported: await name("dual"),
        other: await name("dual-other"),
        loaded: short(loaded?.importedIds?.[0]),
      };
      return code + "\nexport const seen = " + JSON.stringify(seen) + ";\n";
    },
  },
];
"#;

fn dual(name: &str) -> String {
    format!(r#"{{"name":"{name}","version":"1.0.0","main":"server.js","browser":"browser.js"}}"#)
}

#[tokio::test(flavor = "multi_thread")]
async fn ctx_resolve_and_load_use_the_environments_own_resolver() {
    let (dual_pkg, other_pkg) = (dual("dual"), dual("dual-other"));
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"ctxenv","private":true,"type":"module"}"#,
        ),
        ("node_modules/dual/package.json", &dual_pkg),
        ("node_modules/dual/server.js", "export default 'server';\n"),
        (
            "node_modules/dual/browser.js",
            "export default 'browser';\n",
        ),
        ("node_modules/dual-other/package.json", &other_pkg),
        (
            "node_modules/dual-other/server.js",
            "export default 'server';\n",
        ),
        (
            "node_modules/dual-other/browser.js",
            "export default 'browser';\n",
        ),
        (
            "src/main.js",
            "import dual from \"dual\";\nexport const value = dual;\n",
        ),
        (
            "src/dep.js",
            "import dual from \"dual\";\nexport default dual;\n",
        ),
        ("index.html", INDEX_HTML),
        ("oj.plugins.mjs", PLUGINS),
    ]);
    let server = app.serve().await;
    let compact = |s: String| s.split_whitespace().collect::<String>();

    let client = compact(server.text("/src/main.js").await);
    assert!(
        client.contains(
            r#"{"imported":"dual/browser.js","other":"dual-other/browser.js","loaded":"dual/browser.js"}"#
        ),
        "the client resolves with the browser field:\n{client}"
    );

    let main = app.root.join("src/main.js");
    let ssr = compact(
        server
            .text(&format!("/@ssr-module?id={}", main.display()))
            .await,
    );
    assert!(
        ssr.contains(
            r#"{"imported":"dual/server.js","other":"dual-other/server.js","loaded":"dual/server.js"}"#
        ),
        "the ssr environment resolves without the browser field:\n{ssr}"
    );
}
