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

/// Vite's devHtmlHook runs an inline `<style>` through the CSS plugin
/// container (plugin transforms, `@import` inlining, aliased and relative
/// urls) and a `style` attribute holding a url through it too, in index.html
/// and in a page a middleware renders.
#[tokio::test(flavor = "multi_thread")]
async fn inline_styles_compile_like_a_stylesheet_beside_the_page() {
    // In vite.config.js, which also carries the alias (an oj.plugins.mjs
    // would take its place).
    const STYLED: &str = r#"export default { resolve: { alias: { "@": SRC } }, plugins: [{
    name: "css-mark",
    transform(code, id) {
      if (id.includes("html-proxy") && id.endsWith(".css")) return code + "\n.from-plugin { color: red }\n";
    },
  }, {
    name: "styled-page",
    configureServer(server) {
      server.middlewares.use(async (req, res, next) => {
        if (!req.url.startsWith("/__styled/")) return next();
        const html = `<html><head><style>.p { background: url(@/bg.png) } .r { background: url(./bg.png) }</style></head><body></body></html>`;
        res.setHeader("content-type", "text/html");
        res.end(await server.transformIndexHtml(req.url, html));
      });
    },
  }] };
"#;
    let app = App::new(&[
        ("package.json", r#"{"name":"styled-app","private":true}"#),
        ("src/base.css", ".base { margin: 0 }\n"),
        (
            "index.html",
            "<!doctype html><html><head>\n\
             <style>@import \"./src/base.css\";\n.hero { background: url(@/bg.png) }</style>\n\
             </head><body>\n\
             <div style=\"background: url(./src/bg.png)\"></div>\n\
             <p style=\"color: red\"></p>\n\
             <i style=\"background: url(./missing.png)\"></i>\n\
             </body></html>",
        ),
    ]);
    std::fs::write(app.root.join("src/bg.png"), [7u8; 4]).unwrap();
    // Beside the middleware page's path: Vite still never resolves a
    // relative url from a page with no file.
    std::fs::create_dir_all(app.root.join("__styled")).unwrap();
    std::fs::write(app.root.join("__styled/bg.png"), [7u8; 4]).unwrap();
    let src = serde_json::to_string(&app.root.join("src").to_string_lossy()).unwrap();
    app.write(&[("vite.config.js", STYLED.replace("SRC", &src).as_str())]);
    let server = app.serve().await;

    let html = server.text("/").await;
    let sheet = &html[html.find("<style>").expect("style kept")..html.find("</style>").unwrap()];
    assert!(sheet.contains(".base"), "the @import is inlined:\n{html}");
    assert!(!sheet.contains("@import"), "{html}");
    assert!(
        sheet.contains("/src/bg.png"),
        "the aliased url points at the file:\n{html}"
    );
    assert!(
        sheet.contains(".from-plugin"),
        "plugin transforms run:\n{html}"
    );
    assert!(
        html.contains("style=\"background: url(/src/bg.png)\""),
        "a style attribute's relative url is rebased:\n{html}"
    );
    assert!(
        html.contains("<p style=\"color: red\">"),
        "an attribute with no url stays as written:\n{html}"
    );
    assert!(
        html.contains("<i style=\"background: url(./missing.png)\">"),
        "a url naming no file stays as written:\n{html}"
    );

    middleware_ready(&server).await;
    let page = server.get("/__styled/Card").await;
    assert_eq!(page.status, 200, "{}", page.body);
    assert!(
        page.body.contains("/src/bg.png") && page.body.contains(".from-plugin"),
        "a middleware page's style compiles too:\n{}",
        page.body
    );
    assert!(
        page.body.contains("url(\"./bg.png\")"),
        "a page with no file keeps its relative urls:\n{}",
        page.body
    );
}

/// A `style` attribute goes through the app's PostCSS plugins before its urls
/// are rewritten (Vite's PostCSS pass, url rewrite last): a url a plugin
/// writes is rebased too, and the declarations otherwise stay as written.
#[tokio::test(flavor = "multi_thread")]
async fn a_style_attribute_runs_the_apps_postcss_plugins_first() {
    crate::require_fixture!("postcss");
    let app = App::new(&[
        ("package.json", r#"{"name":"attr-postcss","private":true}"#),
        ("oj.plugins.mjs", "export default [{ name: \"noop\" }];\n"),
        ("src/main.js", "export {};\n"),
        (
            "postcss.config.mjs",
            "export default { plugins: [{ postcssPlugin: \"icon\", Declaration: { \"--icon\": (decl) => { decl.value = \"url(./src/bg.png)\"; } } }] };\n",
        ),
        (
            "index.html",
            "<html><head></head><body>\
             <div style=\"color:red;background: url(./src/bg.png); --icon: none\"></div>\
             </body></html>",
        ),
    ]);
    std::fs::write(app.root.join("src/bg.png"), [7u8; 4]).unwrap();
    let node_modules = crate::fixture("postcss")
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    std::os::unix::fs::symlink(node_modules, app.root.join("node_modules")).unwrap();
    let server = app.serve().await;

    let html = server.text("/").await;
    assert!(
        html.contains("style=\"color:red;background: url(/src/bg.png); --icon: url(/src/bg.png)\""),
        "{html}"
    );
}
