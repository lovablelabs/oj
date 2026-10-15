// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Vite unwraps every hook's object form `{ order, filter, handler }` and
//! sorts a hook's plugins by `order`; a string id filter is a glob joined to
//! the root; `configEnvironment` runs per environment; `hotUpdate` receives
//! `server`. Each used to be skipped or mis-matched. This is the dev half;
//! the build half (buildStart object form, the glob-filtered transform in the
//! bundle) needs the build harness.

use std::path::Path;
use std::time::Duration;

use crate::plugin_middleware::middleware_ready;
use crate::{App, INDEX_HTML};

const PLUGINS: &str = r#"import fs from "node:fs";
const mark = (n, v = "1") => fs.writeFileSync(__MARKS__ + "/" + n, String(v));
export default [
  {
    name: "second",
    config: { order: "post", handler(c) { mark("config-order", (fs.existsSync(__MARKS__ + "/config-first") ? "first-then-second" : "second-first")); return { define: { __SECOND__: "2" } }; } },
  },
  {
    name: "first",
    config: { handler() { mark("config-first"); return { define: { __FIRST__: "1" } }; } },
    configResolved: { handler(rc) { mark("configResolved", JSON.stringify({ first: rc.define.__FIRST__, second: rc.define.__SECOND__ })); } },
    configEnvironment: { handler(name, opts) { mark("env-" + name, JSON.stringify(opts)); return { define: { __ENV__: JSON.stringify(name) } }; } },
    configureServer: { handler(server) { server.middlewares.use("/__objhook", (req, res) => { res.setHeader("content-type", "text/plain"); res.end("middleware-ok"); }); } },
    transform: { filter: { id: "src/**/*.js" }, handler(code, id) { return code + "\nglobalThis.__T = \"TRANSFORMED " + id.replace(/\\/g, "/").split("/").pop() + "\";\n"; } },
    hotUpdate: { handler(ctx) { mark("hotUpdate", JSON.stringify({ file: ctx.file.split("/").pop(), hasServer: !!(ctx.server && ctx.server.ws && typeof ctx.server.ws.send === "function"), type: ctx.type })); } },
    buildStart: { handler() { mark("buildStart"); } },
  },
];
"#;

fn read(marks: &Path, name: &str) -> String {
    std::fs::read_to_string(marks.join(name))
        .unwrap_or_else(|e| panic!("mark {name} missing ({e})"))
}

#[tokio::test(flavor = "multi_thread")]
async fn object_form_hooks_order_filter_and_server_context_in_dev() {
    // Outside the app root so the watcher never sees the mark files themselves.
    let marks_dir = tempfile::tempdir().unwrap();
    let marks = marks_dir.path().canonicalize().unwrap();
    let plugins = PLUGINS.replace(
        "__MARKS__",
        &serde_json::to_string(&marks.display().to_string()).unwrap(),
    );
    let app = App::new(&[
        ("package.json", r#"{"name":"objhooks","version":"1.0.0"}"#),
        ("src/deep/a.js", "export const A = \"a\";\n"),
        ("other.js", "export const O = \"o\";\n"),
        (
            "src/main.js",
            "import \"./deep/a.js\";\nimport \"../other.js\";\nwindow.__ok = 1;\n",
        ),
        ("index.html", INDEX_HTML),
        ("oj.plugins.mjs", &plugins),
    ]);
    let server = app.serve().await;

    assert_eq!(
        read(&marks, "config-order"),
        "first-then-second",
        "config order:post ran after the normal hook"
    );
    let resolved: serde_json::Value =
        serde_json::from_str(&read(&marks, "configResolved")).unwrap();
    assert_eq!(
        resolved,
        serde_json::json!({ "first": "1", "second": "2" }),
        "configResolved object form saw both config results"
    );
    assert!(
        marks.join("env-client").exists() && marks.join("env-ssr").exists(),
        "configEnvironment ran per environment"
    );

    middleware_ready(&server).await;
    assert_eq!(
        server.text("/__objhook").await,
        "middleware-ok",
        "configureServer object form registered its middleware"
    );

    let a = server.text("/src/deep/a.js").await;
    assert!(
        a.contains("TRANSFORMED a.js"),
        "glob id filter should match src/deep/a.js:\n{a}"
    );
    let other = server.text("/other.js").await;
    assert!(
        !other.contains("TRANSFORMED"),
        "glob id filter must not match a file outside src/:\n{other}"
    );

    app.write(&[("src/deep/a.js", "export const A = \"a2\";\n")]);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !marks.join("hotUpdate").exists() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        marks.join("hotUpdate").exists(),
        "hotUpdate never dispatched for the edit"
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    // The mark is written in one call but may be observed mid-write.
    let hot: serde_json::Value = loop {
        if let Ok(v) = serde_json::from_str(&read(&marks, "hotUpdate")) {
            break v;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "hotUpdate mark never parsed"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(
        hot["file"], "a.js",
        "hotUpdate object form was dispatched: {hot}"
    );
    assert_eq!(
        hot["hasServer"], true,
        "hotUpdate context carries server.ws.send: {hot}"
    );
}
