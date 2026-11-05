// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Route code splitting, as TanStack Start does it under Vite: its router
//! plugin runs the code splitter in each environment (client and server), so
//! a route file keeps its options and imports its component lazily from
//! `route.tsx?tsr-split=component`, compiled out of the same file. A route
//! with `ssr: false` whose component imports a browser-only library (one
//! touching `window` as it loads) never loads that library on the server:
//! every page renders in dev and from the build, the route's page renders in
//! the browser, and the build gives the route its own chunk on both sides.
//! Before, the route tree imported the component with its route, so the
//! server evaluated the library and every page answered 500.
//!
//! The app is the Start fixture (see `start_app`) moved to file-based
//! routing. The browser half skips without Chrome, unless
//! `OJ_REQUIRE_BROWSER` is set.

#[path = "../../oj_server/tests/dev/browser.rs"]
#[allow(dead_code)]
mod browser;
#[allow(dead_code)]
mod start_app;

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use browser::Page;
use start_app::{app, build, Server};

const ROOT: &str = r#"import { createRootRoute, HeadContent, Outlet, Scripts } from "@tanstack/react-router";

export const Route = createRootRoute({
  head: () => ({ meta: [{ title: "split routes" }] }),
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

export const Route = createFileRoute("/")({
  component: () => <p id="home">home page</p>,
});
"#;

const MAP: &str = r#"import { createFileRoute } from "@tanstack/react-router";
import { mapLabel } from "../lib/browser-map";

export const Route = createFileRoute("/map")({
  ssr: false,
  component: MapPage,
});

function MapPage() {
  return <p id="map">{mapLabel()}</p>;
}
"#;

/// A browser-only library: it reads `window` as it loads.
const BROWSER_MAP: &str = r#"const agent = window.navigator.userAgent;

export const mapLabel = () => (agent ? "browser-map-loaded" : "no agent");
"#;

/// A route whose component and loader share a binding: the splitter moves it
/// to a module both import (`route.tsx?tsr-shared=1`).
const SHARED: &str = r#"import { createFileRoute } from "@tanstack/react-router";

const greeting = "shared-greeting";

export const Route = createFileRoute("/shared")({
  loader: () => greeting.length,
  component: SharedPage,
});

function SharedPage() {
  return <p id="shared">{greeting}</p>;
}
"#;

const ROUTER: &str = r#"import { createRouter } from "@tanstack/react-router";
import { routeTree } from "./routeTree.gen";

export function getRouter() {
  return createRouter({ routeTree });
}
"#;

fn split_app(name: &str) -> Option<start_app::App> {
    let app = app(name)?;
    let root = &app.0;
    let routes = root.join("src/routes");
    std::fs::remove_dir_all(&routes).unwrap();
    std::fs::create_dir_all(&routes).unwrap();
    std::fs::create_dir_all(root.join("src/lib")).unwrap();
    std::fs::write(routes.join("__root.tsx"), ROOT).unwrap();
    std::fs::write(routes.join("index.tsx"), INDEX).unwrap();
    std::fs::write(routes.join("map.tsx"), MAP).unwrap();
    std::fs::write(routes.join("shared.tsx"), SHARED).unwrap();
    std::fs::write(root.join("src/lib/browser-map.ts"), BROWSER_MAP).unwrap();
    std::fs::write(root.join("src/routeTree.ts"), "export {};\n").unwrap();
    std::fs::write(root.join("src/router.tsx"), ROUTER).unwrap();
    Some(app)
}

/// The pages render on the server, the map page without its component.
fn assert_pages_render(server: &Server) {
    let (status, body) = server.request("GET", "/", &[]);
    assert!(
        status == 200 && body.contains("home page"),
        "the home page answered {status} {body:?}\n{}",
        server.output()
    );
    let (status, body) = server.request("GET", "/map", &[]);
    assert_eq!(status, 200, "the map page {body:?}\n{}", server.output());
    assert!(
        !server.output().contains("window is not defined"),
        "the server loaded the browser-only library\n{}",
        server.output()
    );
}

/// The map page renders its component in the browser.
async fn assert_map_renders_in_the_browser(server: &Server) {
    let page = Page::open(&server.url("/map")).await;
    let rendered = page
        .poll(
            r##"document.querySelector("#map")?.textContent === "browser-map-loaded""##,
            Duration::from_secs(30),
        )
        .await;
    let errors = page.errors();
    page.close().await;
    assert!(
        rendered,
        "the map page did not render: {errors:?}\n{}",
        server.output()
    );
}

/// The files under `dir` (recursively) whose text holds `needle`.
fn files_holding(dir: &Path, needle: &str) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.is_dir() {
            out.extend(files_holding(&path, needle));
        } else if std::fs::read_to_string(&path).is_ok_and(|s| s.contains(needle)) {
            out.push(path.file_name().unwrap().to_string_lossy().into_owned());
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_only_route_component_stays_off_the_server_in_dev() {
    let Some(app) = split_app("route-split-dev") else {
        return;
    };
    let dev = Server::oj_dev(&app.0);
    assert_pages_render(&dev);
    if browser::skip() {
        return;
    }
    assert_map_renders_in_the_browser(&dev).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_route_component_gets_its_own_chunk_in_the_build() {
    let Some(app) = split_app("route-split-build") else {
        return;
    };
    let root = &app.0;
    build(root);
    // Vite's build names the chunk after the route file (`map-<hash>.js`).
    let client = files_holding(&root.join("dist/client"), "browser-map-loaded");
    assert!(
        client.len() == 1 && client[0].starts_with("map-"),
        "the client's map component: {client:?}"
    );
    let server = files_holding(&root.join("dist/chunks"), "browser-map-loaded");
    assert!(
        server.len() == 1 && server[0].starts_with("map-"),
        "the server's map component: {server:?}"
    );
    let served = Server::node_server(root);
    assert_pages_render(&served);
    if browser::skip() {
        return;
    }
    assert_map_renders_in_the_browser(&served).await;
}

/// The chunks (by name, hash and extension off) under `dir` holding `needle`,
/// the client entry's as `entry`: oj names it `client`, Vite's build `index`
/// (that naming is not the splitter's).
fn chunk_names(dir: &Path, needle: &str, entry: &str) -> BTreeSet<String> {
    files_holding(dir, needle)
        .into_iter()
        .map(|file| {
            let stem = file.split('.').next().unwrap_or(&file).to_string();
            // A hash may hold `-` itself; these chunk names never do.
            let name = match stem.split_once('-') {
                Some((name, _hash)) => name.to_string(),
                None => stem,
            };
            if name == entry {
                "entry".to_string()
            } else {
                name
            }
        })
        .collect()
}

/// Each route's own code lands in the chunks Vite's build puts it in, on the
/// client and on the server: the split-off component in a chunk named after
/// its route, what it shares with the route's loader beside it, the route
/// tree's code in the entry's.
#[test]
fn routes_split_into_the_chunks_vites_build_makes() {
    let Some(oj_app) = split_app("route-split-oj") else {
        return;
    };
    let Some(vite_app) = split_app("route-split-vite") else {
        return;
    };
    build(&oj_app.0);
    let vite = Command::new("node")
        .arg(vite_app.0.join("node_modules/vite/bin/vite.js"))
        .arg("build")
        .current_dir(&vite_app.0)
        .output()
        .unwrap();
    assert!(
        vite.status.success(),
        "vite build failed:\n{}{}",
        String::from_utf8_lossy(&vite.stdout),
        String::from_utf8_lossy(&vite.stderr)
    );
    for needle in ["browser-map-loaded", "shared-greeting", "home page"] {
        let oj_client = chunk_names(&oj_app.0.join("dist/client"), needle, "client");
        let vite_client = chunk_names(&vite_app.0.join("dist/client"), needle, "index");
        assert!(
            !vite_client.is_empty(),
            "{needle}: Vite's client holds it nowhere"
        );
        assert_eq!(
            oj_client, vite_client,
            "{needle}: client chunks, oj vs Vite"
        );
        let oj_server = chunk_names(&oj_app.0.join("dist/chunks"), needle, "");
        let vite_server = chunk_names(&vite_app.0.join("dist/server/assets"), needle, "");
        assert!(
            !vite_server.is_empty(),
            "{needle}: Vite's server holds it nowhere"
        );
        assert_eq!(
            oj_server, vite_server,
            "{needle}: server chunks, oj vs Vite"
        );
    }
}
