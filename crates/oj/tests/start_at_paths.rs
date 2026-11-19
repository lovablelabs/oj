// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A path whose first segment starts with `@` (`/@someone`, a route param
//! that keeps its `@`) reaches the app under `oj dev`, as under Vite, which
//! tries only its own `/@` urls first (`isInternalRequest`: `/@fs/`, `/@id/`,
//! `/@vite/client`, `/@vite/env`), hands a miss on to the app and never
//! answers a page navigation to its module urls. A TanStack Start app's
//! `/$handle` route renders for `/@someone`, for `/@id/posts` or
//! `/@oj/posts` (nothing there), with more segments, a query or the `@`
//! escaped, and the router navigates to it; `oj dev --ssr` renders such a
//! path too. oj's own endpoints stay oj's.
//!
//! The Start app is a file-routed app of its own on the Start fixture's
//! installed `node_modules` (see `start_app::app_from`); without the fixture
//! that test skips, unless `OJ_REQUIRE_FIXTURES` is set. The browser half
//! skips without Chrome, unless `OJ_REQUIRE_BROWSER` is set.

#[path = "../../oj_server/tests/dev/browser.rs"]
#[allow(dead_code)]
mod browser;
#[allow(dead_code)]
mod start_app;

use std::time::Duration;

use browser::Page;
use start_app::Server;

const ROOT: &str = r#"import { createRootRoute, HeadContent, Outlet, Scripts } from "@tanstack/react-router";

export const Route = createRootRoute({
  head: () => ({ meta: [{ title: "handles" }] }),
  component: () => (
    <html lang="en">
      <head>
        <HeadContent />
      </head>
      <body>
        <Outlet />
        <Scripts />
      </body>
    </html>
  ),
});
"#;

const INDEX: &str = r#"import { createFileRoute, Link } from "@tanstack/react-router";

export const Route = createFileRoute("/")({
  component: () => (
    <main>
      <p id="home">home page</p>
      <Link id="to-handle" to="/$handle" params={{ handle: "@someone" }}>someone</Link>
    </main>
  ),
});
"#;

const HANDLE: &str = r#"import { createFileRoute } from "@tanstack/react-router";

export const Route = createFileRoute("/$handle/")({
  component: Handle,
});

function Handle() {
  const { handle } = Route.useParams();
  return <p id="handle">{`handle:${handle}`}</p>;
}
"#;

const POSTS: &str = r#"import { createFileRoute } from "@tanstack/react-router";

export const Route = createFileRoute("/$handle/posts")({
  component: Posts,
});

function Posts() {
  const { handle } = Route.useParams();
  return <p id="posts">{`posts:${handle}`}</p>;
}
"#;

const ROUTER: &str = r#"import { createRouter } from "@tanstack/react-router";
import { routeTree } from "./routeTree.gen";

export function getRouter() {
  return createRouter({ routeTree });
}
"#;

fn handle_app(name: &str) -> Option<start_app::App> {
    let files: &[(&str, String)] = &[
        (
            "package.json",
            r#"{ "name": "at-paths", "private": true, "type": "module", "dependencies": { "@tanstack/react-start": "*", "@tanstack/react-router": "*", "react": "*", "react-dom": "*" } }"#.to_string(),
        ),
        (
            "tsconfig.json",
            r#"{ "compilerOptions": { "jsx": "react-jsx", "module": "ESNext", "moduleResolution": "Bundler", "target": "ES2022", "strict": true, "skipLibCheck": true } }"#.to_string(),
        ),
        (
            "vite.config.ts",
            r#"import { tanstackStart } from "@tanstack/react-start/plugin/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

export default defineConfig({
  plugins: [tanstackStart(), react()],
});
"#
            .to_string(),
        ),
        ("src/router.tsx", ROUTER.to_string()),
        ("src/routes/__root.tsx", ROOT.to_string()),
        ("src/routes/index.tsx", INDEX.to_string()),
        ("src/routes/$handle/index.tsx", HANDLE.to_string()),
        ("src/routes/$handle/posts.tsx", POSTS.to_string()),
    ];
    start_app::app_from(name, files)
}

#[tokio::test(flavor = "multi_thread")]
async fn an_at_prefixed_path_renders_the_start_route_it_matches() {
    let Some(app) = handle_app("at-paths") else {
        return;
    };
    let dev = Server::oj_dev(&app.0);
    let html = [("accept", "text/html".to_string())];

    let (status, home) = dev.request("GET", "/", &html);
    assert!(
        status == 200 && home.contains("home page"),
        "GET /: {status}\n{home}\n{}",
        dev.output()
    );
    for (path, marker) in [
        ("/someone", "handle:someone"),
        ("/@someone", "handle:@someone"),
        ("/@someone/posts", "posts:@someone"),
        ("/@someone?tab=1", "handle:@someone"),
        ("/%40someone", "handle:@someone"),
    ] {
        let (status, body) = dev.request("GET", path, &html);
        assert!(
            status == 200 && body.contains(marker),
            "GET {path}: {status}, no {marker:?} in the server's render\n{body}\n{}",
            dev.output()
        );
    }
    let (status, refresh) = dev.request("GET", "/@react-refresh", &[]);
    assert!(
        status == 200 && !refresh.contains("<html"),
        "oj's own url is not the app's: {status}\n{refresh}"
    );

    // A page request to a `/@` url oj has nothing at renders the app.
    let page = [
        ("accept", "text/html".to_string()),
        ("sec-fetch-dest", "document".to_string()),
    ];
    for (path, marker) in [
        ("/@id/posts", "posts:@id"),
        ("/@fs/posts", "posts:@fs"),
        ("/%40id/posts", "posts:@id"),
        ("/@oj/posts", "posts:@oj"),
        ("/@oj-start/posts", "posts:@oj-start"),
    ] {
        for headers in [&html[..], &page[..]] {
            let (status, body) = dev.request("GET", path, headers);
            assert!(
                status == 200 && body.contains(marker),
                "GET {path} {headers:?}: {status}, no {marker:?} in the server's render\n{body}"
            );
        }
    }
    let (status, _) = dev.request("HEAD", "/@id/posts", &page);
    assert_eq!(status, 200, "HEAD /@id/posts as a page");
    // A module request oj misses there keeps oj's 404 (a swept chunk's
    // answer, which the pages are told of), never the app's render.
    let script = [("sec-fetch-dest", "script".to_string())];
    for path in [
        "/@oj-start/posts",
        "/@oj-start/missing-chunk.js",
        "/@oj/posts",
    ] {
        let (status, body) = dev.request("GET", path, &script);
        assert!(
            status == 404 && !body.contains("<html"),
            "GET {path} as a module: {status}\n{body}"
        );
    }
    // A page navigation to one of Vite's module urls is the app's too.
    let (status, body) = dev.request("GET", "/@react-refresh", &page);
    assert!(
        status == 200 && body.contains("handle:@react-refresh"),
        "/@react-refresh as a page: {status}\n{body}"
    );
    let (status, body) = dev.request("GET", "/@vite/client", &page);
    assert!(
        status == 404 && body.contains("<html"),
        "/@vite/client as a page is the app's not-found page: {status}\n{body}"
    );
    // oj's own endpoints answer whatever asks.
    for headers in [&page[..], &html[..], &script[..], &[][..]] {
        let (status, _, body) = dev.fetch("GET", "/@oj/client.js", headers);
        let body = String::from_utf8_lossy(&body);
        assert!(
            status == 200 && !body.contains("<html") && body.contains("import"),
            "/@oj/client.js {headers:?}: {status}\n{body}"
        );
        if std::path::Path::new("/etc/passwd").is_file() {
            let (status, body) = dev.request("GET", "/@fs/etc/passwd", headers);
            assert_eq!(status, 403, "/@fs/etc/passwd {headers:?}: {body}");
        }
    }

    if browser::skip() {
        return;
    }
    let page = Page::open(&dev.url("/")).await;
    let hydrated = page
        .poll(
            r#"document.getElementById("home") && !!self.__TSR_ROUTER__"#,
            Duration::from_secs(60),
        )
        .await;
    assert!(hydrated, "/ never hydrated: {:?}", page.errors());
    page.eval::<bool>(r##"(document.querySelector("#to-handle").click(), true)"##)
        .await;
    let navigated = page
        .poll(
            r#"document.getElementById("handle")?.textContent === "handle:@someone" && decodeURIComponent(location.pathname) === "/@someone""#,
            Duration::from_secs(30),
        )
        .await;
    let errors = page.errors();
    page.close().await;
    assert!(navigated, "the router never reached /@someone: {errors:?}");
}

#[test]
fn an_at_prefixed_path_renders_under_ssr_dev() {
    let root = std::env::temp_dir().join(format!("oj-ssr-at-paths-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("package.json"),
        r#"{"name":"ssr-at-paths","private":true,"type":"module"}"#,
    )
    .unwrap();
    std::fs::write(
        root.join("src/entry-server.js"),
        "export function render(url) {\n  return `<p>ssr-path:${url}</p>`;\n}\n",
    )
    .unwrap();
    let dev = Server::oj_dev_with_args(&root, &["--ssr", "src/entry-server.js"], &[]);
    let html = [("accept", "text/html".to_string())];
    for (path, marker) in [
        ("/someone", "ssr-path:/someone"),
        ("/@someone", "ssr-path:/@someone"),
        ("/@someone/posts", "ssr-path:/@someone/posts"),
        ("/@someone?tab=1", "ssr-path:/@someone"),
        ("/@id/posts", "ssr-path:/@id/posts"),
        ("/@fs/posts", "ssr-path:/@fs/posts"),
        ("/@oj/posts", "ssr-path:/@oj/posts"),
    ] {
        let (status, body) = dev.request("GET", path, &html);
        assert!(
            status == 200 && body.contains(marker),
            "GET {path}: {status}, no {marker:?}\n{body}\n{}",
            dev.output()
        );
    }
    let (status, refresh) = dev.request("GET", "/@react-refresh", &[]);
    assert!(
        status == 200 && !refresh.contains("ssr-path"),
        "oj's own url is not rendered: {status}\n{refresh}"
    );
    let page = [
        ("accept", "text/html".to_string()),
        ("sec-fetch-dest", "document".to_string()),
    ];
    for path in ["/@id/posts", "/@vite/client", "/@react-refresh"] {
        let (status, body) = dev.request("GET", path, &page);
        assert!(
            status == 200 && body.contains(&format!("ssr-path:{path}")),
            "GET {path} as a page: {status}\n{body}"
        );
    }
    let (status, _) = dev.request("HEAD", "/@id/posts", &page);
    assert_eq!(status, 200, "HEAD /@id/posts as a page");
    // A module request to a `/@` url is the dev server's, as Vite's transform
    // middleware serves it before the app's handler; a page renders.
    let script = [("sec-fetch-dest", "script".to_string())];
    let (status, body) = dev.request("GET", "/@my-plugin-runtime", &script);
    assert!(
        !body.contains("ssr-path") && !body.contains("<html"),
        "/@my-plugin-runtime as a module: {status}\n{body}"
    );
    let (status, body) = dev.request("GET", "/@my-plugin-runtime", &page);
    assert!(
        status == 200 && body.contains("ssr-path:/@my-plugin-runtime"),
        "/@my-plugin-runtime as a page: {status}\n{body}"
    );
    // The default allowed hosts render; a host the dev server does not
    // allow gets no render (Vite's hostValidationMiddleware runs first).
    for host in ["localhost", "127.0.0.1"] {
        let headers = [
            ("accept", "text/html".to_string()),
            ("host", format!("{host}:{}", dev.port)),
        ];
        let (status, body) = dev.request("GET", "/@id/posts", &headers);
        assert!(
            status == 200 && body.contains("ssr-path:/@id/posts"),
            "GET /@id/posts from {host}: {status}\n{body}"
        );
    }
    for path in ["/someone", "/@id/posts"] {
        let headers = [
            ("accept", "text/html".to_string()),
            ("host", "attacker.example".to_string()),
        ];
        let (status, body) = dev.request("GET", path, &headers);
        assert!(
            status == 403 && !body.contains("ssr-path"),
            "GET {path} from a disallowed host: {status}\n{body}"
        );
    }
    drop(dev);
    let _ = std::fs::remove_dir_all(&root);
}
