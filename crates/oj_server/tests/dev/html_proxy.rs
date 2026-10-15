// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Vite's dev HTML transform end to end: an inline `<script type="module">`
//! becomes an html-proxy module whose imports resolve like any module's, both
//! in index.html and in a page a configureServer middleware renders through
//! `server.transformIndexHtml` (the shape that left bare `react` imports in a
//! component preview), and a SPA fallback rebases the page's relative urls.
//! Byte-level parity with Vite is e2e/unit/vite-html.test.mjs.

use axum::body::Body;
use axum::http::Request;

use crate::plugin_middleware::middleware_ready;
use crate::App;

const PLUGINS: &str = r#"export default [{
    name: "preview-page",
    configureServer(server) {
      server.middlewares.use(async (req, res, next) => {
        if (!req.url.startsWith("/__preview/")) return next();
        const html = `<!doctype html><html><head><title>p</title></head><body>
<div id="root"></div>
<script type="module">
  import { mark } from "/src/mark.js";
  document.getElementById("root").textContent = mark;
</script>
</body></html>`;
        res.setHeader("content-type", "text/html");
        res.end(await server.transformIndexHtml(req.url, html));
      });
    },
  }];
"#;

fn src_of_proxy(html: &str) -> String {
    let at = html
        .find("html-proxy")
        .unwrap_or_else(|| panic!("no html-proxy script:\n{html}"));
    let start = html[..at].rfind("src=\"").expect("proxy src") + 5;
    let end = start + html[start..].find('"').expect("src end");
    html[start..end].to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_inline_module_in_index_html_is_served_as_an_html_proxy_module() {
    let app = App::new(&[
        ("package.json", r#"{"name":"proxy-app","private":true}"#),
        ("oj.plugins.mjs", "export default [{ name: \"noop\" }];\n"),
        ("src/mark.js", "export const mark = \"inline-ok\";\n"),
        (
            "index.html",
            "<!doctype html><html><head><title>t</title></head><body>\n\
             <script type=\"module\">\n  import { mark } from \"./src/mark.js\";\n  console.log(mark);\n</script>\n\
             </body></html>",
        ),
    ]);
    let server = app.serve().await;
    let html = server.text("/").await;
    assert_eq!(
        src_of_proxy(&html),
        "/index.html?html-proxy&index=0.js",
        "{html}"
    );
    assert!(
        !html.contains("import { mark }"),
        "the inline body was lifted out:\n{html}"
    );
    assert_eq!(
        html.matches("/@oj/client.js").count(),
        1,
        "one dev client:\n{html}"
    );

    let module = server.get("/index.html?html-proxy&index=0.js").await;
    assert_eq!(module.status, 200, "{}", module.body);
    assert!(
        module.body.contains("\"/src/mark.js\""),
        "the relative import resolves from the page's directory:\n{}",
        module.body
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_middleware_page_gets_the_dev_client_and_a_loadable_proxy_module() {
    let app = App::new(&[
        ("package.json", r#"{"name":"preview-app","private":true}"#),
        ("oj.plugins.mjs", PLUGINS),
        ("src/mark.js", "export const mark = \"preview-ok\";\n"),
        (
            "index.html",
            "<!doctype html><html><head></head><body></body></html>",
        ),
    ]);
    let server = app.serve().await;
    middleware_ready(&server).await;

    let page = server.get("/__preview/Badge").await;
    assert_eq!(page.status, 200, "{}", page.body);
    assert!(
        page.body.contains("/@oj/client.js"),
        "dev client injected:\n{}",
        page.body
    );
    // The page has no file: its proxy is a `\0` id behind /@id/, as in Vite.
    let src = src_of_proxy(&page.body);
    assert_eq!(src, "/@id/__x00__/__preview/Badge?html-proxy&index=0.js");

    let module = server.get(&src).await;
    assert_eq!(module.status, 200, "{}", module.body);
    assert!(module.body.contains("\"/src/mark.js\""), "{}", module.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_spa_fallback_rebases_the_pages_relative_urls() {
    let app = App::new(&[
        ("package.json", r#"{"name":"spa-app","private":true}"#),
        ("oj.plugins.mjs", "export default [{ name: \"noop\" }];\n"),
        ("src/main.js", "export {};\n"),
        (
            "index.html",
            "<!doctype html><html><head></head><body>\
             <script type=\"module\" src=\"./src/main.js\"></script></body></html>",
        ),
    ]);
    let server = app.serve().await;
    let page = server
        .request(
            Request::get("/dashboard/settings")
                .header("accept", "text/html")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(page.status, 200, "{}", page.body);
    assert!(
        page.body.contains("src=\"/src/main.js\""),
        "a nested route's ./ url points at the root, not /dashboard/:\n{}",
        page.body
    );
}
