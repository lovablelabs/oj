// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A plugin's `this.resolve` resolves with its own environment's resolve
//! options, as Vite builds its resolve plugin per environment
//! (`applyToEnvironment`, plugins/resolve.ts): the ssr environment never takes
//! a package's `browser` field. Both of oj's paths are covered: an import of
//! the transforming module (pre-resolved before the hook runs) and a
//! specifier it does not import (resolved on demand). `this.load`'s
//! `importedIds`, an oj field (Vite's dev ModuleInfo has only `id` and
//! `meta`), resolve the same way.

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

const COND_PKG: &str = r#"{"name":"cond","version":"1.0.0","exports":{"custom":"./custom.js","top":"./top.js","default":"./default.js"}}"#;

/// Reports what `this.resolve("cond")` answers for `src/main.js`.
const REPORT_COND: &str = r#"{
  name: "reports-cond",
  configEnvironment(name) {
    if (name === "ssr") return { resolve: { conditions: ["custom"] } };
  },
  async transform(code, id) {
    if (!id.split("?")[0].endsWith("/src/main.js")) return null;
    const r = await this.resolve("cond", id);
    return code + "\nexport const cond = " + JSON.stringify(r ? r.id.split("/").pop() : null) + ";\n";
  },
}"#;

// Through Vite's own resolved config: a `configEnvironment` hook's ssr
// conditions reach that environment's `this.resolve` (reported by the host,
// absent from oj's extracted config), while the top-level
// `resolve.conditions` apply to the client only (getDefaultEnvironmentOptions).
#[tokio::test(flavor = "multi_thread")]
async fn the_hosts_reported_environment_options_and_top_level_conditions() {
    crate::require_fixture!("vite");
    let config = format!(
        "export default {{ plugins: [{REPORT_COND}], resolve: {{ conditions: [\"top\"] }} }};\n"
    );
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"ctxreported","private":true,"type":"module"}"#,
        ),
        ("node_modules/cond/package.json", COND_PKG),
        ("node_modules/cond/custom.js", "export default 'custom';\n"),
        ("node_modules/cond/top.js", "export default 'top';\n"),
        (
            "node_modules/cond/default.js",
            "export default 'default';\n",
        ),
        ("src/main.js", "export const value = 1;\n"),
        ("index.html", INDEX_HTML),
        ("vite.config.mjs", &config),
    ]);
    app.link_fixture("vite");
    let server = app.serve().await;
    let client = server.text("/src/main.js").await;
    assert!(client.contains(r#"cond = "top.js""#), "client: {client}");
    let main = app.root.join("src/main.js");
    let ssr = server
        .text(&format!("/@ssr-module?id={}", main.display()))
        .await;
    assert!(ssr.contains(r#"cond = "custom.js""#), "ssr: {ssr}");
}

// Vite's resolve plugin clears its cache on a created or deleted file
// (watchChange); an environment's resolver must not keep a miss either.
#[tokio::test(flavor = "multi_thread")]
async fn an_environments_resolver_forgets_a_miss_once_the_file_exists() {
    const LATER: &str = r#"export default [
  {
    name: "reports-later",
    async transform(code, id) {
      if (!id.split("?")[0].endsWith("/src/main.js")) return null;
      const r = await this.resolve("./later.js", id);
      return code + "\nexport const later = " + JSON.stringify(r ? "found" : "missing") + ";\n";
    },
  },
];
"#;
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"ctxlater","private":true,"type":"module"}"#,
        ),
        ("src/main.js", "export const value = 1;\n"),
        ("index.html", INDEX_HTML),
        ("oj.plugins.mjs", LATER),
    ]);
    let server = app.serve().await;
    let main = app.root.join("src/main.js");
    let url = format!("/@ssr-module?id={}", main.display());
    let first = server.text(&url).await;
    assert!(first.contains(r#"later = "missing""#), "{first}");

    app.write(&[("src/later.js", "export default 1;\n")]);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut n = 2;
    loop {
        // An edit re-runs the transform; the created file drops the misses.
        app.write(&[("src/main.js", &format!("export const value = {n};\n"))]);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let now = server.text(&url).await;
        if now.contains(r#"later = "found""#) {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "still a miss: {now}");
        n += 1;
    }
}
