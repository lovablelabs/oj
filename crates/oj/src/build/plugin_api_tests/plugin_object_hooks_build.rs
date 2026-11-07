// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The build half of the object-form hook check: Vite unwraps
//! `{ order, filter, handler }` on every hook, so the object-form `buildStart`
//! runs in the build and a string id filter is a glob joined to the root
//! (`src/**/*.js` matches src/deep/a.js, not other.js).

use super::{build_app, files_under, scratch, write_files};

/// The plugin file; `@MARKS@` is the JSON-quoted marks dir.
const PLUGINS: &str = r#"import fs from "node:fs";
const mark = (n, v = "1") => fs.writeFileSync(@MARKS@ + "/" + n, String(v));
export default [
  {
    name: "second",
    config: { order: "post", handler(c) { mark("config-order", (fs.existsSync(@MARKS@ + "/config-first") ? "first-then-second" : "second-first")); return { define: { __SECOND__: "2" } }; } },
  },
  {
    name: "first",
    config: { handler() { mark("config-first"); return { define: { __FIRST__: "1" } }; } },
    configResolved: { handler(rc) { mark("configResolved", JSON.stringify({ first: rc.define.__FIRST__, second: rc.define.__SECOND__ })); } },
    configEnvironment: { handler(name, opts) { mark("env-" + name, JSON.stringify(opts)); return { define: { __ENV__: JSON.stringify(name) } }; } },
    configureServer: { handler(server) { server.middlewares.use("/__objhook", (req, res) => { res.setHeader("content-type", "text/plain"); res.end("middleware-ok"); }); } },
    // A side effect (not a comment) so the marker survives the minifier.
    transform: { filter: { id: "src/**/*.js" }, handler(code, id) { return code + "\nglobalThis.__T = \"TRANSFORMED " + id.replace(/\\/g, "/").split("/").pop() + "\";\n"; } },
    hotUpdate: { handler(ctx) { mark("hotUpdate", JSON.stringify({ file: ctx.file.split("/").pop(), hasServer: !!(ctx.server && ctx.server.ws && typeof ctx.server.ws.send === "function"), type: ctx.type })); } },
    buildStart: { handler() { mark("buildStart"); } },
  },
];
"#;

#[tokio::test]
async fn object_form_build_start_and_glob_filtered_transform_run_in_the_build() {
    let root = scratch("objhooks-build");
    // Outside the app root, so the marks never join the graph.
    let marks = scratch("objhooks-build-marks");
    let plugins = PLUGINS.replace(
        "@MARKS@",
        &serde_json::to_string(&marks.display().to_string()).unwrap(),
    );
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"objhooks","version":"1.0.0"}"#),
            ("src/deep/a.js", "export const A = \"a\";\n"),
            ("other.js", "export const O = \"o\";\n"),
            (
                "src/main.js",
                "import \"./deep/a.js\";\nimport \"../other.js\";\nwindow.__ok = 1;\n",
            ),
            (
                "index.html",
                r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>"#,
            ),
            ("oj.plugins.mjs", plugins.as_str()),
        ],
    );
    if let Err(err) = build_app(&root).await {
        panic!("the build failed:\n{err:#}");
    }

    // Only the build ran, so this mark is the build's.
    assert!(
        marks.join("buildStart").exists(),
        "buildStart object form ran in the build"
    );
    let built = files_under(&root.join("dist/assets"))
        .into_iter()
        .map(|(_, code)| code)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        built.contains("TRANSFORMED a.js"),
        "build applied the glob-filtered transform:\n{built}"
    );
    assert!(
        !built.contains("TRANSFORMED other.js"),
        "glob id filter must not match a file outside src/:\n{built}"
    );
}
