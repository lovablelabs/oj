// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A file route's `server` handlers stay out of the client, as Start's router
//! plugin does it in Vite: the client code splitter deletes the route options
//! `ssr`, `server` and `headers` (its `deleteNodes`) and drops what only they
//! used. A server route importing a `*.server.ts` module, with import
//! protection set to fail (`behavior: "error"`), hydrates in dev and builds;
//! the server still answers the
//! route with its handler. Before, the client bundled the handler and its
//! import: import protection threw in dev, the client entry answered 404 and
//! the page never hydrated.
//!
//! The client's route tree leaves a server-only route out, as Start's
//! route-tree-client-plugin prunes it (`pruneServerOnlySubtrees`), and takes
//! it in once an edit gives it a component.
//!
//! The app is the Start fixture (see `start_app`) moved to file-based
//! routing. The browser half skips without Chrome, unless
//! `OJ_REQUIRE_BROWSER` is set.

#[path = "../../oj_server/tests/dev/browser.rs"]
#[allow(dead_code)]
mod browser;
#[allow(dead_code)]
mod start_app;

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use browser::Page;
use start_app::{app, Server};

const ROOT: &str = r#"import { createRootRoute, HeadContent, Outlet, Scripts } from "@tanstack/react-router";

export const Route = createRootRoute({
  head: () => ({ meta: [{ title: "file routes" }] }),
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
"#;

const INDEX: &str = r#"import { createFileRoute } from "@tanstack/react-router";
import { useEffect, useState } from "react";

export const Route = createFileRoute("/")({
  component: Home,
});

function Home() {
  const [state, setState] = useState("ssr");
  useEffect(() => setState("hydrated"), []);
  return <p id="hydrated">{state}</p>;
}
"#;

/// A server route whose handler calls a server-only module through a helper.
const HOOK: &str = r#"import { createFileRoute } from "@tanstack/react-router";
import { secret } from "../../server/hook-secret.server";

function answer() {
  return new Response(secret());
}

export const Route = createFileRoute("/api/hook")({
  server: {
    handlers: {
      GET: async () => answer(),
    },
  },
});
"#;

const ROUTER: &str = r#"import { createRouter } from "@tanstack/react-router";
import { routeTree } from "./routeTree.gen";

export function getRouter() {
  return createRouter({ routeTree });
}
"#;

fn file_routed(name: &str) -> Option<start_app::App> {
    let app = app(name)?;
    let root = &app.0;
    let routes = root.join("src/routes");
    std::fs::remove_dir_all(&routes).unwrap();
    std::fs::create_dir_all(routes.join("api")).unwrap();
    std::fs::write(routes.join("__root.tsx"), ROOT).unwrap();
    std::fs::write(routes.join("index.tsx"), INDEX).unwrap();
    std::fs::write(routes.join("api/hook.ts"), HOOK).unwrap();
    std::fs::write(root.join("src/routeTree.ts"), "export {};\n").unwrap();
    std::fs::write(root.join("src/router.tsx"), ROUTER).unwrap();
    std::fs::write(
        root.join("src/server/hook-secret.server.ts"),
        "export const secret = () => \"hook-server-secret\";\n",
    )
    .unwrap();
    let config = root.join("vite.config.ts");
    let source = std::fs::read_to_string(&config).unwrap();
    let call = "tanstackStart({ server: { entry: \"ssr-entry\" } })";
    assert!(source.contains(call), "{source}");
    let with = "tanstackStart({ server: { entry: \"ssr-entry\" }, importProtection: { behavior: \"error\" } })";
    std::fs::write(&config, source.replacen(call, with, 1)).unwrap();
    Some(app)
}

/// `oj build`: whether it succeeded, and its output.
fn build(root: &Path) -> (bool, String) {
    let _ = std::fs::remove_dir_all(root.join(".oj-cache"));
    let out = Command::new(env!("CARGO_BIN_EXE_oj"))
        .arg("build")
        .arg(root)
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

fn client_has(root: &Path, needle: &str) -> bool {
    fn walk(dir: &Path, needle: &str) -> bool {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        entries.flatten().any(|e| {
            let path = e.path();
            if path.is_dir() {
                walk(&path, needle)
            } else {
                std::fs::read_to_string(&path).is_ok_and(|s| s.contains(needle))
            }
        })
    }
    walk(&root.join("dist/client"), needle)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_routes_handlers_stay_out_of_the_client_in_dev() {
    let Some(app) = file_routed("route-server-dev") else {
        return;
    };
    let dev = Server::oj_dev(&app.0);
    let (status, body) = dev.request("GET", "/api/hook", &[]);
    assert!(
        status == 200 && body.contains("hook-server-secret"),
        "the server route answered {status} {body:?}\n{}",
        dev.output()
    );
    let (status, _) = dev.request("GET", "/@oj-start/client-entry.js", &[]);
    assert_eq!(status, 200, "the client entry\n{}", dev.output());
    assert!(
        !dev.output().contains("[import-protection]"),
        "import protection saw the route's server handler\n{}",
        dev.output()
    );
    if browser::skip() {
        return;
    }
    let page = Page::open(&dev.url("/")).await;
    let hydrated = page
        .poll(
            r##"document.querySelector("#hydrated")?.textContent === "hydrated""##,
            Duration::from_secs(30),
        )
        .await;
    let errors = page.errors();
    page.close().await;
    assert!(
        hydrated,
        "the page did not hydrate: {errors:?}\n{}",
        dev.output()
    );
    assert_eq!(errors, Vec::<String>::new(), "page errors");
}

#[test]
fn a_server_routes_handlers_stay_out_of_the_client_build() {
    let Some(app) = file_routed("route-server-build") else {
        return;
    };
    let (ok, text) = build(&app.0);
    assert!(ok, "the build failed:\n{text}");
    assert!(!text.contains("[import-protection]"), "{text}");
    assert!(
        !client_has(&app.0, "hook-server-secret"),
        "the client holds the route's server handler"
    );
    assert!(
        !client_has(&app.0, "/api/hook"),
        "the client's route tree holds the server-only route"
    );
}

/// The hook route with a component as well as its handlers: no longer
/// server-only.
const HOOK_WITH_PAGE: &str = r#"import { createFileRoute } from "@tanstack/react-router";
import { secret } from "../../server/hook-secret.server";

export const Route = createFileRoute("/api/hook")({
  server: {
    handlers: {
      GET: async () => new Response(secret()),
    },
  },
  component: () => <p id="hook-page">hook page</p>,
});
"#;

/// The client router's route ids, once the page hydrated.
async fn client_route_ids(dev: &Server) -> Vec<String> {
    let page = Page::open(&dev.url("/")).await;
    let hydrated = page
        .poll(
            r##"document.querySelector("#hydrated")?.textContent === "hydrated""##,
            Duration::from_secs(30),
        )
        .await;
    assert!(
        hydrated,
        "the page did not hydrate: {:?}\n{}",
        page.errors(),
        dev.output()
    );
    let ids: Vec<String> = page
        .eval("Object.keys(self.__TSR_ROUTER__?.routesById ?? {})")
        .await;
    page.close().await;
    ids
}

#[tokio::test(flavor = "multi_thread")]
async fn the_client_route_tree_leaves_a_server_only_route_out_until_it_has_a_page() {
    let Some(app) = file_routed("route-server-tree") else {
        return;
    };
    if browser::skip() {
        return;
    }
    let dev = Server::oj_dev(&app.0);
    let ids = client_route_ids(&dev).await;
    assert!(ids.iter().any(|id| id == "/"), "{ids:?}");
    assert!(
        !ids.iter().any(|id| id == "/api/hook"),
        "the client's tree holds the server-only route: {ids:?}"
    );

    std::fs::write(app.0.join("src/routes/api/hook.tsx"), HOOK_WITH_PAGE).unwrap();
    std::fs::remove_file(app.0.join("src/routes/api/hook.ts")).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let ids = client_route_ids(&dev).await;
        if ids.iter().any(|id| id == "/api/hook") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the route with a page never reached the client's tree: {ids:?}\n{}",
            dev.output()
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_edit_that_gives_a_server_route_a_page_reaches_the_client_tree() {
    let Some(app) = file_routed("route-server-edit") else {
        return;
    };
    if browser::skip() {
        return;
    }
    let dev = Server::oj_dev(&app.0);
    let ids = client_route_ids(&dev).await;
    assert!(!ids.iter().any(|id| id == "/api/hook"), "{ids:?}");
    // The same file, edited in place: the route set does not change.
    let hook = app.0.join("src/routes/api/hook.ts");
    std::fs::write(
        &hook,
        HOOK_WITH_PAGE.replace(
            "component: () => <p id=\"hook-page\">hook page</p>,",
            "component: () => null,",
        ),
    )
    .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let ids = client_route_ids(&dev).await;
        if ids.iter().any(|id| id == "/api/hook") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the edited route never reached the client's tree: {ids:?}\n{}",
            dev.output()
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
