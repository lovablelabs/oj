// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A plugin's configureServer middleware under the semantics of Vite's
//! `server.middlewares`, a connect stack (connect's `proto.use` and
//! `proto.handle`): one mounted on a route (`use("/api", fn)`) runs for the
//! route's own path and the paths under it, case-insensitively and only at
//! a segment boundary, with `req.url` stripped of the route and the
//! browser's url in `req.originalUrl`, and the url is whole again for the
//! middlewares after it. And a plugin that maps files in `buildStart` and
//! serves them from a middleware its configureServer post hook registers
//! (vite-plugin-static-copy's shape) answers them from the first request.

use crate::plugin_middleware::middleware_ready;
use crate::{App, INDEX_HTML};

/// A route-mounted handler that reports what it saw, one mounted with a
/// trailing slash, and a root handler after them that reports the url it
/// sees once the route-mounted one passed the request on.
const ROUTED: &str = r#"export default [{
  name: "routed",
  configureServer(server) {
    server.middlewares.use("/__api", (req, res, next) => {
      if (req.url.startsWith("/pass")) {
        req.seenByRoute = req.url;
        return next();
      }
      res.setHeader("content-type", "application/json");
      res.setHeader("x-routed", "1");
      res.end(JSON.stringify({ url: req.url, originalUrl: req.originalUrl }));
    });
    server.middlewares.use("/__trail/", (req, res) => {
      res.setHeader("content-type", "text/plain");
      res.end("trail:" + req.url);
    });
    server.middlewares.use((req, res, next) => {
      if (!req.url.startsWith("/__api/pass")) return next();
      res.setHeader("content-type", "application/json");
      res.end(JSON.stringify({ url: req.url, seenByRoute: req.seenByRoute }));
    });
  },
}];
"#;

fn routed_app() -> App {
    App::new(&[
        ("package.json", r#"{"name":"mw-routes","version":"1.0.0"}"#),
        ("oj.plugins.mjs", ROUTED),
        ("src/main.js", "console.log(1);\n"),
        ("index.html", INDEX_HTML),
    ])
}

fn json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("a JSON reply ({e}):\n{body}"))
}

/// connect's `handle`: the handler of `use("/__api", fn)` reads the url past
/// its route (`/users/7?page=2` for `/__api/users/7?page=2`, `/` for the
/// route alone, `/?q=1` with a query), the browser's url in
/// `req.originalUrl`, the match ignores case and takes `.` as a boundary,
/// and `/__apiary` is not under `/__api`.
#[tokio::test(flavor = "multi_thread")]
async fn a_route_mounted_middleware_sees_the_url_past_its_route() {
    let app = routed_app();
    let server = app.serve().await;
    middleware_ready(&server).await;

    let res = server.get("/__api/users/7?page=2").await;
    assert_eq!(res.status, 200, "{}", res.body);
    assert_eq!(
        json(&res.body),
        serde_json::json!({ "url": "/users/7?page=2", "originalUrl": "/__api/users/7?page=2" }),
        "the handler reads req.url with its route stripped and the browser's url in req.originalUrl"
    );
    assert_eq!(
        json(&server.text("/__api").await),
        serde_json::json!({ "url": "/", "originalUrl": "/__api" }),
        "the route alone leaves `/`"
    );
    assert_eq!(
        json(&server.text("/__api?q=1").await)["url"],
        "/?q=1",
        "the query stays on the stripped url"
    );
    assert_eq!(
        json(&server.text("/__API/Users").await)["url"],
        "/Users",
        "the route matches case-insensitively, the rest keeps its case"
    );
    assert_eq!(
        json(&server.text("/__api.json").await)["url"],
        "/.json",
        "a `.` after the route is a boundary"
    );

    let res = server.get("/__apiary.txt").await;
    assert_eq!(
        (res.status, res.headers.get("x-routed")),
        (404, None),
        "a path that only begins with the route's text is not under it:\n{}",
        res.body
    );
}

/// connect restores `req.url` before the next entry runs, so a root
/// middleware after a route-mounted one that called `next()` sees the whole
/// url; and `use("/__trail/", fn)` mounts on `/__trail` (connect's `use`
/// drops the trailing slash), taking `/__trail` itself too.
#[tokio::test(flavor = "multi_thread")]
async fn the_url_is_whole_again_after_a_route_mounted_middleware_passes() {
    let app = routed_app();
    let server = app.serve().await;
    middleware_ready(&server).await;

    assert_eq!(
        json(&server.text("/__api/pass/x?y=1").await),
        serde_json::json!({ "url": "/__api/pass/x?y=1", "seenByRoute": "/pass/x?y=1" }),
        "the route-mounted handler saw the stripped url, the root one after it the whole url"
    );
    assert_eq!(server.text("/__trail/a").await, "trail:/a");
    assert_eq!(
        server.text("/__trail").await,
        "trail:/",
        "a route given with a trailing slash takes its bare path too"
    );
}

/// vite-plugin-static-copy's serve plugin: the files to serve are collected
/// in `buildStart`, the middleware is registered by the function
/// configureServer returns and then moved ahead of Vite's public-file
/// middleware in `server.middlewares.stack` (which has no such entry here).
const COPY_LIKE: &str = r#"import fs from "node:fs";
import path from "node:path";

let config;
const fileMap = new Map();
export default [{
  name: "copy-like:serve",
  apply: "serve",
  configResolved(c) {
    config = c;
  },
  buildStart() {
    fileMap.clear();
    const dir = path.join(config.root, "vendor/assets");
    for (const name of fs.readdirSync(dir)) fileMap.set("/copied/" + name, path.join(dir, name));
  },
  configureServer({ middlewares }) {
    return () => {
      const middleware = function copyLikeMiddleware(req, res, next) {
        const pathname = req.url.split("?")[0];
        const file = fileMap.get(pathname);
        if (!file) return next();
        res.setHeader("content-type", pathname.endsWith(".json") ? "application/json" : "application/octet-stream");
        fs.createReadStream(file).pipe(res);
      };
      middlewares.use(middleware);
      const target = middlewares.stack.findIndex((m) => m.handle.name === "viteServePublicMiddleware");
      const own = middlewares.stack.findIndex((m) => m.handle === middleware);
      const item = middlewares.stack.splice(own, 1)[0];
      if (item === undefined) throw new Error("the registered middleware is in the stack");
      middlewares.stack.splice(target, 0, item);
    };
  },
}];
"#;

/// The copied files answer from the first request, with the plugin's body
/// and content type, and a path the map lacks is oj's own 404.
#[tokio::test(flavor = "multi_thread")]
async fn files_mapped_in_build_start_are_served_by_the_post_hook_middleware() {
    let app = App::new(&[
        ("package.json", r#"{"name":"mw-copy","version":"1.0.0"}"#),
        ("oj.plugins.mjs", COPY_LIKE),
        ("src/main.js", "console.log(1);\n"),
        ("index.html", INDEX_HTML),
        ("vendor/assets/terrain.json", r#"{"height":1}"#),
        ("vendor/assets/model.bin", "model-bytes"),
    ]);
    let server = app.serve().await;
    middleware_ready(&server).await;

    let res = server.get("/copied/terrain.json").await;
    assert_eq!(
        (res.status, res.body.as_str()),
        (200, r#"{"height":1}"#),
        "a file mapped in buildStart is served"
    );
    assert_eq!(
        res.headers
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("application/json")
    );
    assert_eq!(server.text("/copied/model.bin").await, "model-bytes");

    let res = server.get("/copied/missing.json").await;
    assert_eq!(res.status, 404, "{}", res.body);
    assert!(
        res.body.contains("no such file"),
        "a miss falls through to oj's 404:\n{}",
        res.body
    );
}
