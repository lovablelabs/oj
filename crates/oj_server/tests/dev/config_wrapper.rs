// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A vite.config that delegates to a preset package's `defineConfig`
//! wrapper (imported from node_modules) is evaluated, and what the wrapper
//! adds applies: its `transformIndexHtml` plugin runs on the served html and
//! its `resolve.alias` (`@` to `process.cwd()/src`, the app root) resolves
//! imports in the page.

use std::time::Duration;

use crate::browser::Page;
use crate::App;

const WAIT: Duration = Duration::from_secs(20);

const PRESET: &str = r#"export function defineConfig(user = {}) {
  return {
    ...user,
    resolve: { ...(user.resolve || {}), alias: { "@": process.cwd() + "/src" } },
    plugins: [
      ...(user.plugins || []),
      { name: "oj-preset", transformIndexHtml(h) { return h.replace("</head>", '<script>window.__PRESET = 1;</script></head>'); } },
    ],
  };
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn preset_define_config_wrapper_applies_plugins_and_alias() {
    let app = App::new(&[
        ("package.json", r#"{"name":"wrapper-app","version":"1.0.0"}"#),
        (
            "node_modules/oj-preset/package.json",
            r#"{"name":"oj-preset","version":"1.0.0","type":"module","main":"index.mjs"}"#,
        ),
        ("node_modules/oj-preset/index.mjs", PRESET),
        (
            "vite.config.mjs",
            "import { defineConfig } from \"oj-preset\";\nexport default defineConfig({ plugins: [] });\n",
        ),
        ("src/thing.js", "export const V = \"aliased-ok\";\n"),
        (
            "main.js",
            "import { V } from \"@/thing.js\";\nwindow.__V = V;\nwindow.__READY = true;\n",
        ),
        (
            "index.html",
            r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/main.js"></script></body></html>"#,
        ),
    ]);
    let server = app.serve().await;

    let html = server.text("/").await;
    assert!(
        html.contains("window.__PRESET = 1"),
        "wrapper's transformIndexHtml plugin ran (config-wrapper evaluated):\n{html}"
    );

    if crate::browser::skip() {
        return;
    }
    let listening = server.listen().await;
    let page = Page::open(&format!("http://127.0.0.1:{}/", listening.addr.port())).await;
    page.wait_for("window.__READY === true", WAIT).await;
    assert_eq!(
        page.eval::<String>("window.__V").await,
        "aliased-ok",
        "wrapper's resolve.alias (@ -> src) applied"
    );
    assert_eq!(page.errors(), Vec::<String>::new(), "page errors");
    page.close().await;
}
