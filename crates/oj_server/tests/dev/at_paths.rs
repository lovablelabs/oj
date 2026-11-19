// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Vite tries only its own `/@` urls first (`isInternalRequest`: `/@fs/`,
//! `/@id/`, `/@vite/client`, `/@vite/env`) and hands a miss on to the app;
//! a page navigation (`Sec-Fetch-Dest` in its transform middleware's
//! `documentFetchDests`) skips its module urls altogether. So a navigation to
//! any `/@` path but a file or one of oj's own endpoints gets the SPA
//! fallback like any route, and a source folder named `@x` is a folder like
//! any other.

use std::time::Duration;

use axum::body::Body;
use axum::http::Request;

use crate::{App, Server};

const SHELL: &str = "spa-shell-marker";

fn spa() -> App {
    App::new(&[
        ("package.json", r#"{"name":"at-app","private":true}"#),
        ("src/main.js", "export {};\n"),
        (
            "index.html",
            "<!doctype html><html><head><title>spa-shell-marker</title></head><body>\
             <script type=\"module\" src=\"/src/main.js\"></script></body></html>",
        ),
    ])
}

/// `path` asked for as a page (`document`), by a plain html request
/// (`html`), as a module (`script`) or with no header at all (`none`).
async fn fetch(server: &Server, path: &str, how: &str) -> crate::Response {
    let req = Request::get(path);
    let req = match how {
        "document" => req
            .header("accept", "text/html")
            .header("sec-fetch-dest", "document"),
        "html" => req.header("accept", "text/html"),
        "script" => req
            .header("accept", "*/*")
            .header("sec-fetch-dest", "script"),
        _ => req,
    };
    server.request(req.body(Body::empty()).unwrap()).await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_navigation_under_at_gets_the_spa_fallback() {
    let app = spa();
    let server = app.serve().await;
    for path in [
        "/@someone",
        "/@someone/posts",
        "/@someone?tab=1",
        "/%40someone",
        "/@fsomeone",
    ] {
        let page = fetch(&server, path, "html").await;
        assert_eq!(page.status, 200, "{path}: {}", page.body);
        assert!(page.body.contains(SHELL), "{path}: {}", page.body);
    }

    let refresh = server.get("/@react-refresh").await;
    assert_eq!(refresh.status, 200, "{}", refresh.body);
    assert!(
        !refresh.body.contains(SHELL),
        "oj's own url is not a navigation: {}",
        refresh.body
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_vite_url_that_names_nothing_gets_the_spa_fallback() {
    let app = spa();
    let server = app.serve().await;
    // Nothing resolves `posts`, no `/posts` file exists, `%40` is no `@` to
    // Vite's prefix test, and oj serves nothing at the last two.
    for path in [
        "/@id/posts",
        "/@fs/posts",
        "/%40id/posts",
        "/@oj/posts",
        "/@oj-start/posts",
    ] {
        for how in ["document", "html"] {
            let page = fetch(&server, path, how).await;
            assert!(
                page.status == 200 && page.body.contains(SHELL),
                "{path} ({how}): {} {}",
                page.status,
                page.body
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_module_request_oj_misses_gets_a_404_not_the_shell() {
    let app = spa();
    let server = app.serve().await;
    // Vite's indexHtml middleware serves no page to `Sec-Fetch-Dest: script`.
    for path in [
        "/@id/posts",
        "/@oj/posts",
        "/@fs/posts",
        "/@id/__x00__virtual:gone",
        "/@someone",
        "/missing",
    ] {
        let res = fetch(&server, path, "script").await;
        assert!(
            res.status == 404 && !res.body.contains("<html") && !res.body.contains(SHELL),
            "{path} as a module: {} {}",
            res.status,
            res.body
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_page_navigation_to_a_vite_module_url_gets_the_spa_fallback() {
    let app = spa();
    let server = app.serve().await;
    for path in ["/@vite/client", "/@vite/env", "/@react-refresh"] {
        let page = fetch(&server, path, "document").await;
        assert!(
            page.status == 200 && page.body.contains(SHELL),
            "{path} as a page: {} {}",
            page.status,
            page.body
        );
    }
    // The same urls asked for as modules are still oj's.
    for path in ["/@vite/env", "/@react-refresh"] {
        for how in ["script", "none", "html"] {
            let module = fetch(&server, path, how).await;
            assert!(
                module.status == 200 && !module.body.contains(SHELL),
                "{path} ({how}): {} {}",
                module.status,
                module.body
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_oj_endpoint_answers_a_page_navigation_too() {
    let app = spa();
    let server = app.serve().await;
    for path in [
        "/@oj/client.js",
        "/@oj/refresh-runtime.js",
        "/@oj/refresh-preamble.js",
    ] {
        for how in ["document", "html", "script", "none"] {
            let res = fetch(&server, path, how).await;
            assert!(
                res.status == 200
                    && !res.body.contains(SHELL)
                    && res
                        .headers
                        .get("content-type")
                        .and_then(|v| v.to_str().ok())
                        .is_some_and(|t| t.contains("javascript")),
                "{path} ({how}): {} {:?} {}",
                res.status,
                res.headers.get("content-type"),
                res.body
            );
        }
    }
    let diagnostics = fetch(&server, "/@oj/diagnostics", "document").await;
    assert!(
        diagnostics.status == 200 && !diagnostics.body.contains(SHELL),
        "/@oj/diagnostics as a page: {} {}",
        diagnostics.status,
        diagnostics.body
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_fs_url_outside_the_allow_list_stays_forbidden() {
    let app = App::new(&[
        ("secret.txt", "outside-the-root\n"),
        ("web/package.json", r#"{"name":"at-app","private":true}"#),
        ("web/src/main.js", "export {};\n"),
        (
            "web/index.html",
            "<!doctype html><title>spa-shell-marker</title>",
        ),
    ]);
    let mut dev = app.dev();
    dev.root = app.root.join("web");
    let server = crate::serve(dev).await;
    let outside = std::fs::canonicalize(app.root.join("secret.txt")).unwrap();
    for how in ["document", "html", "script", "none"] {
        let res = fetch(&server, &format!("/@fs{}", outside.display()), how).await;
        assert!(
            res.status == 403 && !res.body.contains("outside-the-root"),
            "/@fs{} ({how}): {} {}",
            outside.display(),
            res.status,
            res.body
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_fs_url_naming_a_folder_gets_the_spa_fallback() {
    // sirv skips a directory and hands the request on, as for a missing file.
    let app = spa();
    app.write(&[("sub/a.txt", "a\n")]);
    let server = app.serve().await;
    let root = std::fs::canonicalize(&app.root).unwrap();
    for dir in [root.join("sub"), root.clone()] {
        let path = format!("/@fs{}", dir.display());
        for how in ["document", "html"] {
            let page = fetch(&server, &path, how).await;
            assert!(
                page.status == 200 && page.body.contains(SHELL),
                "{path} ({how}): {} {}",
                page.status,
                page.body
            );
        }
        let module = fetch(&server, &path, "script").await;
        assert!(
            module.status == 404 && !module.body.contains(SHELL),
            "{path} as a module: {} {}",
            module.status,
            module.body
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_script_request_for_an_html_page_gets_a_404() {
    // Vite's htmlFallback maps `/page` to `page.html`, which its indexHtml
    // middleware serves to anything but `Sec-Fetch-Dest: script`.
    let app = spa();
    app.write(&[(
        "page.html",
        "<!doctype html><html><head><title>page-html-marker</title></head><body></body></html>",
    )]);
    let server = app.serve().await;
    for how in ["document", "html", "none"] {
        let page = fetch(&server, "/page", how).await;
        assert!(
            page.status == 200 && page.body.contains("page-html-marker"),
            "/page ({how}): {} {}",
            page.status,
            page.body
        );
    }
    let module = fetch(&server, "/page", "script").await;
    assert!(
        module.status == 404 && !module.body.contains("<html"),
        "/page as a module: {} {}",
        module.status,
        module.body
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_updated_module_in_an_at_named_root_folder_is_imported_under_its_stamp() {
    let app = App::new(&[
        ("package.json", r#"{"name":"at-folder","private":true}"#),
        (
            "index.html",
            "<!doctype html><html><head></head><body>\
             <script type=\"module\" src=\"/main.js\"></script></body></html>",
        ),
        (
            "main.js",
            "import { v } from \"./@x/a.js\";\nconsole.log(v);\nimport.meta.hot?.accept();\n",
        ),
        ("@x/a.js", "export const v = \"one\";\n"),
    ]);
    let server = app.serve().await;
    let main = server.text("/main.js").await;
    assert!(main.contains("\"/@x/a.js\""), "{main}");
    server.text("/@x/a.js").await;

    app.write(&[("@x/a.js", "export const v = \"two\";\n")]);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let main = server.text("/main.js").await;
        if main.contains("\"/@x/a.js?t=") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the importer still names the cached url after the edit:\n{main}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// A `transform` that makes a module of a `.note` file, as an MDX plugin does.
const NOTE_PLUGIN: &str = r#"export default [
  {
    name: "note",
    transform: {
      filter: { id: /\.note$/ },
      handler(code) {
        return "export default " + JSON.stringify(code.trim()) + ";\n";
      },
    },
  },
];
"#;

#[tokio::test(flavor = "multi_thread")]
async fn an_updated_plugin_module_in_an_at_named_root_folder_is_imported_under_its_stamp() {
    let app = App::new(&[
        ("package.json", r#"{"name":"at-folder","private":true}"#),
        (
            "index.html",
            "<!doctype html><html><head></head><body>\
             <script type=\"module\" src=\"/main.js\"></script></body></html>",
        ),
        (
            "main.js",
            "import v from \"./@x/a.note\";\nconsole.log(v);\nimport.meta.hot?.accept();\n",
        ),
        ("@x/a.note", "one\n"),
        ("oj.plugins.mjs", NOTE_PLUGIN),
    ]);
    let server = app.serve().await;
    let main = server.text("/main.js").await;
    assert!(main.contains("\"/@x/a.note\""), "{main}");
    let note = server
        .request(
            Request::get("/@x/a.note")
                .header("sec-fetch-dest", "script")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert!(
        note.body.contains("export default \"one\""),
        "{}",
        note.body
    );

    app.write(&[("@x/a.note", "two\n")]);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let main = server.text("/main.js").await;
        if main.contains("\"/@x/a.note?t=") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the importer still names the cached url after the edit:\n{main}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
