// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A plugin's `this.resolve` resolves with its own environment's resolve
//! options, as Vite's resolve plugin reads `this.environment.config.resolve`:
//! the ssr environment never takes a package's `browser` field. Both of oj's
//! paths are covered: an import of the transforming module (pre-resolved
//! before the hook runs) and a specifier it does not import (resolved on
//! demand). Before, both answered with the client resolver in SSR.

use crate::{App, INDEX_HTML};

const PLUGINS: &str = r#"export default [
  {
    name: "reports-resolve",
    async transform(code, id) {
      if (!id.split("?")[0].endsWith("/src/main.js")) return null;
      const name = async (spec) => {
        const r = await this.resolve(spec, id);
        return r ? r.id.split("/").slice(-2).join("/") : null;
      };
      const seen = { imported: await name("dual"), other: await name("dual-other") };
      return code + "\nexport const seen = " + JSON.stringify(seen) + ";\n";
    },
  },
];
"#;

fn dual(name: &str) -> String {
    format!(r#"{{"name":"{name}","version":"1.0.0","main":"server.js","browser":"browser.js"}}"#)
}

#[tokio::test(flavor = "multi_thread")]
async fn ctx_resolve_uses_the_environments_own_resolver() {
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
        ("index.html", INDEX_HTML),
        ("oj.plugins.mjs", PLUGINS),
    ]);
    let server = app.serve().await;
    let compact = |s: String| s.split_whitespace().collect::<String>();

    let client = compact(server.text("/src/main.js").await);
    assert!(
        client.contains(r#"{"imported":"dual/browser.js","other":"dual-other/browser.js"}"#),
        "the client resolves with the browser field:\n{client}"
    );

    let main = app.root.join("src/main.js");
    let ssr = compact(
        server
            .text(&format!("/@ssr-module?id={}", main.display()))
            .await,
    );
    assert!(
        ssr.contains(r#"{"imported":"dual/server.js","other":"dual-other/server.js"}"#),
        "the ssr environment resolves without the browser field:\n{ssr}"
    );
}
