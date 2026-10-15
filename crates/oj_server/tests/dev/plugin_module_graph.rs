// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `server.moduleGraph.getModuleById(id)` + `invalidateModule(mod)` from a
//! plugin drop oj's compiled output for that module, so the next request
//! re-runs plugin transforms, and propagate an HMR message for a real file.
//! Both used to be no-ops on a stub. `server.restart()` re-execs the process,
//! so it stays out of an in-process test.

use std::time::Duration;

use crate::plugin_middleware::middleware_ready;
use crate::{App, INDEX_HTML};

const PLUGINS: &str = r#"let n = 0;
export default [{
  name: "graph-user",
  transform(code, id) {
    if (id.split("?")[0].endsWith("/src/a.js")) return code + "\nglobalThis.__N = " + (++n) + ";\n";
    return null;
  },
  configureServer(server) {
    server.middlewares.use("/__inv", (req, res) => {
      const mod = server.moduleGraph.getModuleById(__A_JS__);
      server.moduleGraph.invalidateModule(mod);
      res.end(mod ? "invalidated" : "no-module");
    });
  },
}];
"#;

#[tokio::test(flavor = "multi_thread")]
async fn invalidate_module_drops_the_cached_compile_and_sends_hmr() {
    let app = App::new(&[
        ("package.json", r#"{"name":"modgraph","version":"1.0.0"}"#),
        ("src/a.js", "export const A = \"a\";\n"),
        ("src/main.js", "import \"./a.js\";\n"),
        ("index.html", INDEX_HTML),
    ]);
    let a_js = app.root.join("src/a.js").display().to_string();
    let plugins = PLUGINS.replace("__A_JS__", &serde_json::to_string(&a_js).unwrap());
    app.write(&[("oj.plugins.mjs", &plugins)]);
    let server = app.serve().await;
    middleware_ready(&server).await;

    let first = server.text("/src/a.js").await;
    assert!(first.contains("__N = 1;"), "first transform:\n{first}");
    let again = server.text("/src/a.js").await;
    assert!(
        again.contains("__N = 1;"),
        "an unchanged module is served from cache:\n{again}"
    );

    let mut frames = server.built.reload_tx.subscribe();
    assert_eq!(
        server.text("/__inv").await,
        "invalidated",
        "getModuleById returned a node"
    );

    // The invalidation reaches the server as an async event; its HMR frame is
    // sent after the cached compile is dropped, so once it arrives the next
    // fetch sees the fresh module.
    let reload = regex::Regex::new(r#""type":"(update|full-reload)""#).unwrap();
    let mut seen = Vec::new();
    let got = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match frames.recv().await {
                Ok(f) if reload.is_match(&f) => return,
                Ok(f) => seen.push(f),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(e) => panic!("ws broadcast closed: {e}"),
            }
        }
    })
    .await;
    assert!(
        got.is_ok(),
        "invalidating a real file propagates an HMR message, got: {}",
        seen.join(" | ")
    );

    let after = server.text("/src/a.js").await;
    assert!(
        after.contains("__N = 2;"),
        "invalidateModule must drop the cached compile so the transform re-runs:\n{after}"
    );
}
