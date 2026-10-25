// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A page no plugin hooks goes through oj's native dev HTML transform, one
//! with a `transformIndexHtml` plugin through the plugin host's; both are
//! Vite's transform, so a hook that changes nothing gives the same bytes and
//! the same lifted module (env values, CSP nonces, the dev client, relative
//! urls of a SPA fallback, inline modules and styles).

use std::time::{Duration, Instant};

use regex::Regex;

use crate::App;

const WAIT: Duration = Duration::from_secs(20);

const PAGE: &str = r#"<!doctype html>
<html>
  <head>
    <title>%VITE_TITLE%</title>
    <link rel="icon" href="./favicon.svg">
    <style>body { margin: 0 }</style>
    <script type="importmap">{"imports":{}}</script>
  </head>
  <body>
    <img src="./a.png" srcset="./a.png 1x, ./a@2x.png 2x">
    <div style="background: url('/bg.png')"></div>
    <script type="module">import { x } from "./src/x.js"; window.__X = x;</script>
    <script>window.lazy = () => import('./src/lazy.js')</script>
  </body>
</html>
"#;

fn app(plugins: &str) -> App {
    App::new(&[
        ("package.json", r#"{"name":"dev-html","version":"1.0.0"}"#),
        (
            "oj.config.json",
            r#"{"html":{"cspNonce":"n0nce"},"appType":"spa"}"#,
        ),
        (".env", "VITE_TITLE=From env\n"),
        ("index.html", PAGE),
        ("src/x.js", "export const x = 1;\n"),
        ("src/lazy.js", "export default 2;\n"),
        ("oj.plugins.mjs", plugins),
    ])
}

async fn page_and_module(app: &App) -> (String, String) {
    let server = app.serve().await;
    // The page's modulepreload links come from the eager crawl, which runs in
    // the background: compare pages once it has listed the inline style.
    let deadline = Instant::now() + WAIT;
    let html = loop {
        let html = server.text("/dashboard/settings").await;
        if html.contains(r#"rel="modulepreload""#) || Instant::now() > deadline {
            break html;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let src = Regex::new(r#"src="([^"]*html-proxy[^"]*)""#)
        .unwrap()
        .captures(&html)
        .unwrap_or_else(|| panic!("an inline module was lifted:\n{html}"))[1]
        .to_string();
    let module = server.text(&src).await;
    // The source map names the app's own (temporary) root.
    let code = module
        .split("//# sourceMappingURL=")
        .next()
        .unwrap()
        .to_string();
    (html, code)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_native_transform_matches_the_hosts() {
    let native =
        app("export default [{ name: \"no-html-hook\", transform() { return null; } }];\n");
    let hosted = app("export default [{ name: \"noop-html-hook\", transformIndexHtml() {} }];\n");
    let (native_html, native_module) = page_and_module(&native).await;
    let (hosted_html, hosted_module) = page_and_module(&hosted).await;
    assert_eq!(native_html, hosted_html);
    assert_eq!(native_module, hosted_module);
    for want in [
        "<title>From env</title>",
        "<meta property=\"csp-nonce\" nonce=\"n0nce\">",
        "src=\"/@oj/client.js\" nonce=\"n0nce\"",
        "href=\"/favicon.svg\"",
        "srcset=\"/a.png 1x, /a@2x.png 2x\"",
        "import('/src/lazy.js')",
        // The style attribute before it takes index 0 (Vite counts both).
        "/index.html?html-proxy&index=1.js",
    ] {
        assert!(native_html.contains(want), "{want}:\n{native_html}");
    }
    assert!(native_module.contains("window.__X"), "{native_module}");
}
