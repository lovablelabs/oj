// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A literal `index.html` path reaches a TanStack Start app under `oj dev`
//! as written, as it does under Vite. Start sets `appType: "custom"`, under
//! which Vite installs neither its html fallback nor its index.html
//! middleware (server/index.ts): the request goes through
//! `servePublicMiddleware` and on to Start's dev middleware, which hands the
//! url as sent (`req.originalUrl`) to the app's handler. So
//! `public/reservas/index.html` is that file, a server route at
//! `/x/index.html` runs its handler, and `/about/index.html` is Start's own
//! 404. oj used to rewrite `/x/index.html` to `/x/` before the app saw it,
//! which Start's trailing-slash handling redirected to `/x`, a path with no
//! route: a 307 and then a 404 where Vite ran the handler. What Start does
//! with a trailing slash (`/about/` to `/about`) is still Start's.
//!
//! The app is a file-routed app of its own on the Start fixture's installed
//! `node_modules` (see `start_app::app_from`); without the fixture the test
//! skips, unless `OJ_REQUIRE_FIXTURES` is set.

#[allow(dead_code)]
mod start_app;

use start_app::Server;

const PUBLIC_INDEX: &str = "<!doctype html><title>reservas</title><p>public-reservas-index</p>\n";

/// A file route on the literal path `/x/index.html` (the router generator's
/// `[.]` escape keeps the dot out of the flat-route separator) with a server
/// handler, answering with a header of its own.
const INDEX_HTML_ROUTE: &str = r#"import { createFileRoute } from "@tanstack/react-router";

export const Route = createFileRoute("/x/index.html")({
  server: {
    handlers: {
      GET: async () =>
        new Response("<!doctype html><p>server-route-index-html</p>", {
          status: 200,
          headers: { "content-type": "text/html", "x-route": "x-index-html" },
        }),
    },
  },
});
"#;

fn index_html_app(name: &str) -> Option<start_app::App> {
    let files: &[(&str, String)] = &[
        (
            "package.json",
            r#"{ "name": "index-html-paths", "private": true, "type": "module", "dependencies": { "@tanstack/react-start": "*", "@tanstack/react-router": "*", "react": "*", "react-dom": "*" } }"#.to_string(),
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
        (
            "src/router.tsx",
            r#"import { createRouter } from "@tanstack/react-router";
import { routeTree } from "./routeTree.gen";

export function getRouter() {
  return createRouter({ routeTree });
}
"#
            .to_string(),
        ),
        (
            "src/routes/__root.tsx",
            r#"import { createRootRoute, HeadContent, Outlet, Scripts } from "@tanstack/react-router";

export const Route = createRootRoute({
  head: () => ({ meta: [{ title: "index.html paths" }] }),
  component: RootComponent,
});

function RootComponent() {
  return (
    <html lang="en">
      <head>
        <HeadContent />
      </head>
      <body>
        <Outlet />
        <Scripts />
      </body>
    </html>
  );
}
"#
            .to_string(),
        ),
        (
            "src/routes/index.tsx",
            r#"import { createFileRoute } from "@tanstack/react-router";

export const Route = createFileRoute("/")({
  component: () => <p id="home">home-page-marker</p>,
});
"#
            .to_string(),
        ),
        (
            "src/routes/about.tsx",
            r#"import { createFileRoute } from "@tanstack/react-router";

export const Route = createFileRoute("/about")({
  component: () => <p id="about">about-page-marker</p>,
});
"#
            .to_string(),
        ),
        ("src/routes/x/index[.]html.tsx", INDEX_HTML_ROUTE.to_string()),
        ("public/reservas/index.html", PUBLIC_INDEX.to_string()),
    ];
    start_app::app_from(name, files)
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

#[test]
fn a_literal_index_html_path_reaches_public_files_and_routes_unchanged() {
    let Some(app) = index_html_app("index-html-paths") else {
        return;
    };
    let server = Server::oj_dev(&app.0);

    // The file under publicDir, as Vite's servePublicMiddleware serves it.
    let (status, _, body) = server.fetch("GET", "/reservas/index.html", &[]);
    let body = String::from_utf8_lossy(&body);
    assert_eq!(status, 200, "{body}\n{}", server.output());
    assert!(body.contains("public-reservas-index"), "{body}");

    // The server route on the literal path runs its handler: no redirect to
    // `/x` (which has no route) on the way.
    for path in ["/x/index.html", "/x/index.html?v=1"] {
        let (status, headers, body) = server.fetch("GET", path, &[]);
        let body = String::from_utf8_lossy(&body);
        assert_eq!(
            status,
            200,
            "GET {path}: location={:?}\n{body}\n{}",
            header(&headers, "location"),
            server.output()
        );
        assert_eq!(
            header(&headers, "x-route"),
            Some("x-index-html"),
            "GET {path}"
        );
        assert!(
            body.contains("server-route-index-html"),
            "GET {path}: {body}"
        );
    }

    // An index.html path with nothing behind it is the app's own 404, as
    // under Vite: not the page of the directory it sits in.
    for path in ["/about/index.html", "/index.html"] {
        let (status, headers, body) = server.fetch("GET", path, &[]);
        let body = String::from_utf8_lossy(&body);
        assert_eq!(status, 404, "GET {path}:\n{body}\n{}", server.output());
        assert_eq!(header(&headers, "location"), None, "GET {path}");
        assert!(!body.contains("about-page-marker"), "GET {path}: {body}");
    }

    // Start's own trailing-slash handling (the router's `trailingSlash`,
    // "never" by default) is still Start's, and the page it points at is
    // served.
    let (status, headers, _) = server.fetch("GET", "/about/", &[]);
    assert_eq!(status, 307, "{}", server.output());
    assert_eq!(header(&headers, "location"), Some("/about"));
    let (status, body) = server.request("GET", "/about", &[]);
    assert_eq!(status, 200, "{body}\n{}", server.output());
    assert!(body.contains("about-page-marker"), "{body}");
}
