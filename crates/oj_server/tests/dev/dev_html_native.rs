// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A page no plugin hooks goes through oj's native dev HTML transform, one
//! with a `transformIndexHtml` plugin through the plugin host's; both are
//! Vite's transform, so a hook that changes nothing gives the same bytes and
//! the same lifted module (env values, CSP nonces, the dev client, relative
//! urls of a SPA fallback, inline modules and styles).

use regex::Regex;

use crate::App;

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
    let html = server.text("/dashboard/settings").await;
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

/// The `modulepreload` links a page carries, in order.
fn preloads(html: &str) -> Vec<String> {
    Regex::new(r#"<link rel="modulepreload" href="([^"]+)""#)
        .unwrap()
        .captures_iter(html)
        .map(|c| c[1].to_string())
        .collect()
}

/// An app with two pages: the root page's script imports a chain, a sheet,
/// JSON (with its attributes) and an image statically, and a lazy route by
/// `import()`; `about.html` has a graph of its own. The root page has an
/// inline style and a modulepreload link of its own.
fn two_page_app(config: &str) -> App {
    App::new(&[
        (
            "package.json",
            r#"{"name":"static-preloads","version":"1.0.0"}"#,
        ),
        ("oj.config.json", config),
        (
            "index.html",
            r#"<!doctype html><html><head><title>t</title><style>body { color: red; }</style><link rel="modulepreload" href="/src/own.js"></head><body><script type="module" src="/src/main.js"></script></body></html>"#,
        ),
        (
            "about.html",
            r#"<!doctype html><html><head><title>about</title></head><body><script type="module" src="/src/about.js"></script></body></html>"#,
        ),
        ("src/own.js", "export const own = 1;\n"),
        ("src/a.js", "export const a = 1;\n"),
        (
            "src/b.js",
            "import { a } from \"./a.js\";\nexport const b = a + 1;\n",
        ),
        ("src/style.css", ".x { color: blue; }\n"),
        ("src/data.json", r#"{ "n": 7 }"#),
        ("src/lazy.js", "export default \"lazy\";\n"),
        (
            "src/about.js",
            "import { a } from \"./a.js\";\nimport \"./only-about.js\";\nwindow.__ABOUT = a;\n",
        ),
        ("src/only-about.js", "export {};\n"),
        (
            "src/main.js",
            "import { b } from \"./b.js\";\n\
             import \"./style.css\";\n\
             import data from \"./data.json\" with { type: \"json\" };\n\
             import pic from \"./pic.png\";\n\
             window.__LAZY = () => import(\"./lazy.js\");\n\
             window.__MAIN = [b, data.n, pic];\n",
        ),
    ])
}

const ROOT_PRELOADS: [&str; 5] = [
    "/src/b.js",
    "/src/data.json",
    "/src/pic.png?url",
    "/src/style.css?import",
    "/src/a.js",
];

fn write_png(app: &App) {
    std::fs::write(app.root.join("src/pic.png"), b"\x89PNG\r\n\x1a\n").unwrap();
}

/// Once the crawl has walked the graph, a page preloads what its module
/// script imports statically, under the URL each import names (a sheet's
/// `?import`, an image's `?url`), and every link serves as the module it
/// names. A module only an `import()` reaches stays out, even after it has
/// loaded; so do the page's inline style (its compile is no module the page
/// imports) and oj's runtime. A link the page writes itself stays first.
///
/// Vite's dev page carries no modulepreload and walks these imports one round
/// trip per level; oj keeps the page's static graph to one round, which over
/// HTTP/2 with latency is the page's import depth in round trips saved. The
/// whole crawled graph (lazy routes, every page's modules) was the old list.
#[tokio::test(flavor = "multi_thread")]
async fn a_dev_page_preloads_its_static_graph_only() {
    let app = two_page_app(r#"{"appType":"mpa"}"#);
    write_png(&app);
    let server = app.serve().await;
    if !crate::crawled(&app.root).await {
        return;
    }
    // The lazy route loads, as a click would load it.
    assert_eq!(server.get("/src/lazy.js").await.status, 200);
    let html = server.text("/").await;
    let mut want = vec!["/src/own.js".to_string()];
    want.extend(ROOT_PRELOADS.iter().map(|s| s.to_string()));
    assert_eq!(preloads(&html), want, "{html}");
    for url in &ROOT_PRELOADS {
        let req = axum::http::Request::get(*url)
            .header("sec-fetch-dest", "script")
            .body(axum::body::Body::empty())
            .unwrap();
        let res = server.request(req).await;
        assert_eq!(res.status, 200, "{url}");
        assert_eq!(res.headers["content-type"], "text/javascript", "{url}");
    }
    // What the next boot preloads before its crawl is done: the same list.
    let snapshot: Vec<String> = serde_json::from_slice(
        &std::fs::read(oj_cache::cache_root(&app.root).join("preload-snapshot.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        snapshot.iter().map(String::as_str).collect::<Vec<_>>(),
        ROOT_PRELOADS
    );
}

/// A page whose script imports files by a query: `?raw` of a Markdown and a
/// CSV file (static imports and an eager glob's members), of a script it
/// also imports as a module, a stylesheet's `?inline`, and a script another
/// module imports as text.
fn queried_imports_app(plugins: Option<&str>) -> App {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"raw-preloads","version":"1.0.0"}"#,
        ),
        (
            "index.html",
            r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>"#,
        ),
        ("src/notes.md", "# Notes\n\nSome *text* with a {brace.\n"),
        ("src/accounts.csv", "id,name\n1,\"Acme, Inc.\"\n"),
        ("src/posts/one.md", "# One\n"),
        ("src/posts/two.md", "# Two\n"),
        ("src/util.js", "export const util = 1;\n"),
        ("src/shared.js", "export const shared = 1;\n"),
        (
            "src/b.js",
            "import sharedText from \"./shared.js?raw\";\nexport const b = sharedText;\n",
        ),
        ("src/style.css", ".x { color: blue; }\n"),
        ("src/main.js", QUERIED_MAIN),
    ]);
    if let Some(plugins) = plugins {
        app.write(&[("oj.plugins.mjs", plugins)]);
    }
    app
}

const QUERIED_MAIN: &str = "import notes from \"./notes.md?raw\";\n\
     import accounts from \"./accounts.csv?raw\";\n\
     import { util } from \"./util.js\";\n\
     import utilText from \"./util.js?raw\";\n\
     import { shared } from \"./shared.js\";\n\
     import { b } from \"./b.js\";\n\
     import css from \"./style.css?inline\";\n\
     const posts = import.meta.glob(\"./posts/*.md\", { query: \"?raw\", import: \"default\", eager: true });\n\
     window.__MAIN = [notes, accounts, util, utilText, shared, b, css, posts];\n";

/// Each import under the url its importer names: `shared.js` is the module
/// for the page's script and `shared.js?raw` for `b.js`, both preloaded.
const QUERIED_PRELOADS: [&str; 10] = [
    "/src/accounts.csv?raw",
    "/src/b.js",
    "/src/notes.md?raw",
    "/src/posts/one.md?raw",
    "/src/posts/two.md?raw",
    "/src/shared.js",
    "/src/style.css?inline",
    "/src/util.js",
    "/src/util.js?raw",
    "/src/shared.js?raw",
];

/// The page's preloads are `QUERIED_PRELOADS` and each answers a script
/// request with a module.
async fn assert_queried_preloads(server: &crate::Server) {
    let html = server.text("/").await;
    let links = preloads(&html);
    assert_eq!(links, QUERIED_PRELOADS, "{html}");
    for url in &links {
        let req = axum::http::Request::get(url.as_str())
            .header("sec-fetch-dest", "script")
            .body(axum::body::Body::empty())
            .unwrap();
        let res = server.request(req).await;
        assert_eq!(res.status, 200, "{url}: {}", res.body);
        assert_eq!(res.headers["content-type"], "text/javascript", "{url}");
    }
}

/// An import named with a query is preloaded under that url, the one its
/// importer asks for: `?raw` of a Markdown or CSV file (a static import and
/// each member of an eager glob), of a script, and a stylesheet's `?inline`.
/// The bare path is another module (the script's own) or no module at all:
/// the page used to preload `/src/notes.md`, which the browser refused as a
/// script. Two modules naming one file by different urls each get theirs.
///
/// Vite's dev page has no preloads: the browser asks only for the urls
/// vite:import-analysis wrote into the importer, each with its query, and
/// vite:asset's load answers `?raw` with the file's text.
#[tokio::test(flavor = "multi_thread")]
async fn a_queried_import_is_preloaded_under_its_query() {
    let app = queried_imports_app(None);
    let server = app.serve().await;
    if !crate::crawled(&app.root).await {
        return;
    }
    assert_queried_preloads(&server).await;
    // The next boot's preloads, before its crawl is done: the same urls.
    let snapshot: Vec<String> = serde_json::from_slice(
        &std::fs::read(oj_cache::cache_root(&app.root).join("preload-snapshot.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(snapshot, QUERIED_PRELOADS);
}

/// The same page in an app with a plugin whose `transform` takes every id,
/// as most apps have: there a script request for the bare `/src/notes.md`
/// goes through the module pipeline, which compiled the Markdown as
/// JavaScript and answered 500 with a parse error naming `notes.md.js`, once
/// per preloaded file, with the error overlay over a page whose imports had
/// all loaded. No preload names a Markdown or CSV file without its query,
/// and every preload answers.
#[tokio::test(flavor = "multi_thread")]
async fn a_queried_import_preload_answers_where_a_plugin_takes_every_id() {
    let app = queried_imports_app(Some(
        "export default [{ name: \"takes-every-id\", transform() { return null; } }];\n",
    ));
    let server = app.serve().await;
    if !crate::crawled(&app.root).await {
        return;
    }
    assert_queried_preloads(&server).await;
    let bare: Vec<String> = preloads(&server.text("/").await)
        .into_iter()
        .filter(|url| url.ends_with(".md") || url.ends_with(".csv"))
        .collect();
    assert_eq!(bare, Vec::<String>::new());
}

/// The page's preloads once `/src/main.js` has been served again, polled
/// until `done` says they are what an edit leaves.
async fn preloads_after_edit(server: &crate::Server, done: impl Fn(&str, &[String]) -> bool) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let main = server.text("/src/main.js").await;
        let links = preloads(&server.text("/").await);
        if done(&main, &links) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the page's preloads never followed the edit: {links:?}\n{main}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// The preloads follow the importer's source: an import it no longer names
/// with a query is no longer preloaded under it, and a file it stops
/// importing is not preloaded at all.
#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_queried_import_is_no_longer_preloaded() {
    let app = queried_imports_app(None);
    let server = app.serve().await;
    if !crate::crawled(&app.root).await {
        return;
    }
    assert_queried_preloads(&server).await;
    app.write(&[(
        "src/main.js",
        &QUERIED_MAIN
            .replace(
                "import notes from \"./notes.md?raw\";\n",
                "const notes = 0;\n",
            )
            .replace(
                "import utilText from \"./util.js?raw\";\n",
                "const utilText = 0;\n",
            ),
    )]);
    preloads_after_edit(&server, |_, links| {
        let gone = |url: &str| !links.iter().any(|l| l == url);
        gone("/src/notes.md?raw")
            && gone("/src/notes.md")
            && gone("/src/util.js?raw")
            && !gone("/src/util.js")
            && !gone("/src/accounts.csv?raw")
    })
    .await;
}

/// After a hot update, an importer names the updated module with the
/// update's stamp (`/src/theme.js?t=..`, Vite's `lastHMRTimestamp` appended
/// by import analysis), and the page's preload of it is that url, never the
/// unstamped one: the preload and the import are one request. Here
/// `widget.js` accepts its own updates and `theme.js`, which it imports, is
/// edited; both are stamped.
#[tokio::test(flavor = "multi_thread")]
async fn a_hot_updated_imports_preload_is_the_url_its_importer_names() {
    let app = App::new(&[
        ("package.json", r#"{"name":"stamped-preloads","version":"1.0.0"}"#),
        (
            "index.html",
            r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>"#,
        ),
        (
            "src/main.js",
            "import { widget } from \"./widget.js\";\nwindow.__MAIN = widget;\n",
        ),
        (
            "src/widget.js",
            "import { theme } from \"./theme.js\";\nexport const widget = theme;\nimport.meta.hot.accept();\n",
        ),
        ("src/theme.js", "export const theme = 1;\n"),
    ]);
    let server = app.serve().await;
    if !crate::crawled(&app.root).await {
        return;
    }
    assert_eq!(
        preloads(&server.text("/").await),
        ["/src/widget.js", "/src/theme.js"]
    );
    app.write(&[("src/theme.js", "export const theme = 2;\n")]);
    let named = |code: &str, file: &str| {
        Regex::new(&format!(r#""(/src/{file}\.js\?t=\d+)""#))
            .unwrap()
            .captures(code)
            .map(|c| c[1].to_string())
    };
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let main = server.text("/src/main.js").await;
        let widget = server.text("/src/widget.js").await;
        let links = preloads(&server.text("/").await);
        if let (Some(widget_url), Some(theme_url)) =
            (named(&main, "widget"), named(&widget, "theme"))
        {
            assert_eq!(links, [widget_url, theme_url]);
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no importer named a stamped url: {links:?}\n{main}\n{widget}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// A snapshot an older version wrote is not read. It named a `?raw` import
/// by its bare path, and the page gets the snapshot's list until the crawl
/// is done, so the first load after an upgrade would preload `/src/notes.md`
/// once more. The crawl is held here by a `transform` of the page's script
/// that waits for a file the test writes; the list this version wrote is
/// what the page preloads meanwhile.
#[tokio::test(flavor = "multi_thread")]
async fn a_snapshot_with_bare_paths_from_an_older_version_is_not_read() {
    let app = queried_imports_app(Some(
        "import { existsSync } from \"node:fs\";\n\
         const release = new URL(\"./release\", import.meta.url);\n\
         export default [{ name: \"held-entry\", async transform(code, id) {\n\
           if (!id.endsWith(\"/src/main.js\")) return null;\n\
           while (!existsSync(release)) await new Promise((r) => setTimeout(r, 50));\n\
           return null;\n\
         } }];\n",
    ));
    let cache = oj_cache::cache_root(&app.root);
    if !cache.starts_with(&app.root) {
        eprintln!("skipping: OJ_CACHE_DIR shares the snapshot");
        return;
    }
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(
        cache.join("graph-snapshot.json"),
        r#"["/src/notes.md","/src/accounts.csv","/src/util.js"]"#,
    )
    .unwrap();
    std::fs::write(
        cache.join("preload-snapshot.json"),
        r#"["/src/notes.md?raw","/src/util.js"]"#,
    )
    .unwrap();
    let server = app.serve().await;
    let html = server.text("/").await;
    assert_eq!(
        preloads(&html),
        ["/src/notes.md?raw", "/src/util.js"],
        "{html}"
    );
    // The crawl replaces the list and removes the older version's.
    app.write(&[("release", "")]);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    while preloads(&server.text("/").await) != QUERIED_PRELOADS {
        assert!(tokio::time::Instant::now() < deadline, "the crawl's list");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(!cache.join("graph-snapshot.json").exists());
}

/// Each page preloads its own graph: `about.html`, once its script has been
/// served, preloads what that script imports and none of the root page's.
#[tokio::test(flavor = "multi_thread")]
async fn each_page_preloads_its_own_graph() {
    let app = two_page_app(r#"{"appType":"mpa"}"#);
    write_png(&app);
    let server = app.serve().await;
    if !crate::crawled(&app.root).await {
        return;
    }
    assert_eq!(
        preloads(&server.text("/about.html").await),
        Vec::<String>::new()
    );
    assert_eq!(server.get("/src/about.js").await.status, 200);
    assert_eq!(server.get("/src/only-about.js").await.status, 200);
    let html = server.text("/about.html").await;
    assert_eq!(
        preloads(&html),
        ["/src/a.js", "/src/only-about.js"],
        "{html}"
    );
}

/// A `cspNonce` page's preloads carry its nonce, as its own links do: the
/// page's policy would block a link without it.
#[tokio::test(flavor = "multi_thread")]
async fn a_nonced_pages_preloads_carry_the_nonce() {
    let app = two_page_app(r#"{"appType":"mpa","html":{"cspNonce":"n0nce"}}"#);
    write_png(&app);
    let server = app.serve().await;
    if !crate::crawled(&app.root).await {
        return;
    }
    let html = server.text("/").await;
    let links: Vec<&str> = html
        .lines()
        .filter(|l| l.contains("rel=\"modulepreload\""))
        .collect();
    assert!(!links.is_empty(), "{html}");
    for link in links {
        assert!(link.contains("nonce=\"n0nce\""), "{link}");
    }
}

/// The page boots on its preloads: every module runs, the JSON import with
/// its attributes included, and no request fails (no preload of a URL that
/// is no module, such as the inline style's proxy).
#[tokio::test(flavor = "multi_thread")]
async fn a_page_boots_on_its_preloads() {
    if crate::browser::skip() {
        return;
    }
    let app = two_page_app(r#"{"appType":"mpa"}"#);
    write_png(&app);
    let server = app.serve().await;
    if !crate::crawled(&app.root).await {
        return;
    }
    let listening = server.listen().await;
    let page =
        crate::browser::Page::open(&format!("http://127.0.0.1:{}/", listening.addr.port())).await;
    let ran = page
        .poll(
            "Array.isArray(window.__MAIN) && window.__MAIN[0] === 2 && window.__MAIN[1] === 7",
            std::time::Duration::from_secs(20),
        )
        .await;
    let failed: Vec<String> = page
        .eval("performance.getEntriesByType('resource').filter((e) => e.responseStatus >= 400).map((e) => e.name)")
        .await;
    let errors = page.errors();
    let console = page.console();
    page.close().await;
    assert!(ran, "the page never ran: {errors:?} {console:?}");
    assert_eq!(errors, Vec::<String>::new(), "page errors");
    assert_eq!(failed, Vec::<String>::new(), "failed requests");
    assert!(
        !console
            .iter()
            .any(|c| c.contains("404") || c.contains("Failed to load")),
        "{console:?}"
    );
}
