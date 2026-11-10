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

/// The hook route loading its server-only module with `await import()` inside
/// the handler, through a tsconfig path. The client splitter deletes
/// `server`, and the dynamic import with it.
const DYNAMIC_HOOK: &str = r#"import { createFileRoute } from "@tanstack/react-router";

export const Route = createFileRoute("/api/hook")({
  server: {
    handlers: {
      ANY: async () => {
        const { secret } = await import("@app/server/hook-secret.server");
        return new Response(secret());
      },
    },
  },
});
"#;

fn dynamic_routed(name: &str) -> Option<start_app::App> {
    let app = file_routed(name)?;
    std::fs::write(app.0.join("src/routes/api/hook.ts"), DYNAMIC_HOOK).unwrap();
    Some(app)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_routes_dynamic_import_stays_out_of_the_client_in_dev() {
    let Some(app) = dynamic_routed("route-server-dynamic-dev") else {
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
        "import protection saw the route's dynamic import\n{}",
        dev.output()
    );
}

#[test]
fn a_server_routes_dynamic_import_stays_out_of_the_client_build() {
    let Some(app) = dynamic_routed("route-server-dynamic-build") else {
        return;
    };
    let (ok, text) = build(&app.0);
    assert!(ok, "the build failed:\n{text}");
    assert!(!text.contains("[import-protection]"), "{text}");
    assert!(
        !client_has(&app.0, "hook-server-secret"),
        "the client holds the route's dynamic import"
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

/// How many times the dev server brought the route tree current (each run
/// prints whether it was restored from the codegen cache or generated).
fn route_tree_runs(dev: &Server) -> usize {
    let out = dev.output();
    out.matches("route tree restored from cache").count()
        + out.matches("route tree cache miss").count()
}

/// Waits until a rebuild for `file` shows in the output.
async fn rebuilt_for(dev: &Server, file: &str, after: usize) {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while dev.output()[after..]
        .lines()
        .all(|l| !(l.contains("rebuilding for") && l.contains(file)))
    {
        assert!(
            std::time::Instant::now() < deadline,
            "no rebuild for {file}\n{}",
            dev.output()
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

// Start reruns its client tree only when an edit flips a route's only-`server`
// shape (its generator plugin's afterTransform): any other route edit leaves
// the tree alone, so a save does not wait on the generator.
#[tokio::test(flavor = "multi_thread")]
async fn only_an_edit_that_flips_a_routes_shape_regenerates_the_tree() {
    let Some(app) = file_routed("route-server-shape") else {
        return;
    };
    if browser::skip() {
        return;
    }
    let dev = Server::oj_dev(&app.0);
    let _ = client_route_ids(&dev).await;
    let runs = route_tree_runs(&dev);

    let index = app.0.join("src/routes/index.tsx");
    let mark = dev.output().len();
    std::fs::write(
        &index,
        INDEX.replace("useState(\"ssr\")", "useState(\"ssr2\")"),
    )
    .unwrap();
    rebuilt_for(&dev, "index.tsx", mark).await;
    assert_eq!(
        route_tree_runs(&dev),
        runs,
        "an edit keeping the route's shape reran the generator\n{}",
        dev.output()
    );

    // A page without JSX: hook.ts is plain TypeScript, which the generator
    // parses without JSX.
    let page = HOOK_WITH_PAGE.replace(
        "component: () => <p id=\"hook-page\">hook page</p>,",
        "component: () => null,",
    );
    std::fs::write(app.0.join("src/routes/api/hook.ts"), page).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while route_tree_runs(&dev) == runs {
        assert!(
            std::time::Instant::now() < deadline,
            "the edit giving the server route a page did not regenerate\n{}",
            dev.output()
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(route_tree_runs(&dev), runs + 1, "{}", dev.output());
}

/// Adds `options` (`key: value, ...`) to the app's `tanstackStart({...})`.
fn start_options(root: &Path, options: &str) {
    let config = root.join("vite.config.ts");
    let source = std::fs::read_to_string(&config).unwrap();
    let call = "importProtection: { behavior: \"error\" } })";
    assert!(source.contains(call), "{source}");
    let with = format!("importProtection: {{ behavior: \"error\" }}, {options} }})");
    std::fs::write(&config, source.replacen(call, &with, 1)).unwrap();
}

// Start puts the route tree where `tanstackStart({ router })` says
// (`<srcDirectory>/<generatedRouteTree>`), and oj puts it there too: one tree,
// and the codegen cache restores that file. Before, oj read tsr.config.json for
// the path, so with the tree configured on Start oj wrote src/routeTree.gen.ts
// while the router imported the configured one.
#[tokio::test(flavor = "multi_thread")]
async fn the_route_tree_goes_where_tanstack_start_puts_it() {
    let Some(app) = file_routed("route-tree-configured") else {
        return;
    };
    start_options(&app.0, "router: { generatedRouteTree: \"routes.gen.ts\" }");
    std::fs::write(
        app.0.join("src/router.tsx"),
        ROUTER.replace("./routeTree.gen", "./routes.gen"),
    )
    .unwrap();
    for boot in ["first", "second"] {
        // A checkout without the generated tree (it is often gitignored) but
        // with the cache: the restore must write the configured tree.
        if boot == "second" {
            std::fs::remove_file(app.0.join("src/routes.gen.ts")).unwrap();
        }
        let dev = Server::oj_dev(&app.0);
        let (status, _) = dev.request("GET", "/", &[]);
        assert_eq!(status, 200, "{boot} boot\n{}", dev.output());
        assert!(
            app.0.join("src/routes.gen.ts").is_file(),
            "{boot} boot: no configured tree\n{}",
            dev.output()
        );
        assert!(
            !app.0.join("src/routeTree.gen.ts").exists(),
            "{boot} boot: a second tree at the default path\n{}",
            dev.output()
        );
        if boot == "second" {
            let out = dev.output();
            assert!(
                out.contains("route tree restored from cache")
                    && !out.contains("route tree cache miss"),
                "the second boot regenerated the tree\n{out}"
            );
        }
    }
}

// Under Start, tsr.config.json does not move the route tree: Start passes its
// own paths to the generator over the file. oj follows, so the tree stays at
// src/routeTree.gen.ts as in Vite.
#[tokio::test(flavor = "multi_thread")]
async fn tsr_config_json_does_not_move_starts_route_tree() {
    let Some(app) = file_routed("route-tree-tsr-config") else {
        return;
    };
    std::fs::write(
        app.0.join("tsr.config.json"),
        r#"{ "generatedRouteTree": "./src/routes.gen.ts" }"#,
    )
    .unwrap();
    let dev = Server::oj_dev(&app.0);
    let (status, _) = dev.request("GET", "/", &[]);
    assert_eq!(status, 200, "{}", dev.output());
    assert!(
        app.0.join("src/routeTree.gen.ts").is_file(),
        "{}",
        dev.output()
    );
    assert!(
        !app.0.join("src/routes.gen.ts").exists(),
        "{}",
        dev.output()
    );
}

// `tanstackStart({ router: { routesDirectory } })` moves the routes: oj reads
// them there, and a route added there reaches the tree.
#[tokio::test(flavor = "multi_thread")]
async fn routes_live_where_tanstack_start_reads_them() {
    let Some(app) = file_routed("route-dir-configured") else {
        return;
    };
    start_options(&app.0, "router: { routesDirectory: \"pages\" }");
    std::fs::rename(app.0.join("src/routes"), app.0.join("src/pages")).unwrap();
    // The hook's server-only import, one level up from src/pages/api.
    let dev = Server::oj_dev(&app.0);
    let (status, body) = dev.request("GET", "/", &[]);
    assert_eq!(status, 200, "{body}\n{}", dev.output());
    std::fs::write(
        app.0.join("src/pages/added.tsx"),
        "import { createFileRoute } from \"@tanstack/react-router\";\n\nexport const Route = createFileRoute(\"/added\")({\n  component: () => <p>added</p>,\n});\n",
    )
    .unwrap();
    let tree = app.0.join("src/routeTree.gen.ts");
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while !std::fs::read_to_string(&tree).is_ok_and(|s| s.contains("./pages/added")) {
        assert!(
            std::time::Instant::now() < deadline,
            "the added route never reached the tree\n{}",
            dev.output()
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // The server picks the new tree up once the rebuild lands.
    loop {
        let (status, body) = dev.request("GET", "/added", &[]);
        if status == 200 && body.contains("added") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the added route never answered: {status}\n{}",
            dev.output()
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// A server route deeper in the routes tree, its handler importing one
/// server-only module statically and another with `await import()`: a
/// hook under `src/routes/api/**` as a webhook worker is written. The
/// client's tree leaves it out, so neither module reaches the browser.
const WORKER: &str = r#"import { createFileRoute } from "@tanstack/react-router";
import { syncRows } from "../../../../server/sheets/sync.server";

export const Route = createFileRoute("/api/public/hooks/jobs-worker")({
  server: {
    handlers: {
      POST: async () => {
        const { gate } = await import("../../../../server/sheets/gate.server");
        return new Response(`${syncRows()}:${gate()}`);
      },
    },
  },
});
"#;

const SYNC_SERVER: &str = r#"import { getRequestHeader } from "@tanstack/react-start/server";

export const syncRows = () => getRequestHeader("x-rows") ?? "worker-sync-secret";
"#;

const GATE_SERVER: &str = "export const gate = () => \"worker-gate-secret\";\n";

fn write_worker(root: &Path) {
    let sheets = root.join("src/server/sheets");
    std::fs::create_dir_all(&sheets).unwrap();
    std::fs::write(sheets.join("sync.server.ts"), SYNC_SERVER).unwrap();
    std::fs::write(sheets.join("gate.server.ts"), GATE_SERVER).unwrap();
    let hooks = root.join("src/routes/api/public/hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    std::fs::write(hooks.join("jobs-worker.ts"), WORKER).unwrap();
}

fn worker_routed(name: &str) -> Option<start_app::App> {
    let app = file_routed(name)?;
    write_worker(&app.0);
    Some(app)
}

/// The worker route answers, the client entry bundles, and import protection
/// says nothing.
fn assert_worker_stays_out_of_the_client(dev: &Server) {
    let (status, body) = dev.request("POST", "/api/public/hooks/jobs-worker", &[]);
    assert!(
        status == 200 && body.contains("worker-sync-secret:worker-gate-secret"),
        "the worker route answered {status} {body:?}\n{}",
        dev.output()
    );
    let (status, _) = dev.request("GET", "/@oj-start/client-entry.js", &[]);
    assert_eq!(status, 200, "the client entry\n{}", dev.output());
    assert!(
        !dev.output().contains("[import-protection]"),
        "import protection saw the worker route's server modules\n{}",
        dev.output()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_nested_server_routes_imports_stay_out_of_the_client_in_dev() {
    let Some(app) = worker_routed("route-worker-dev") else {
        return;
    };
    let dev = Server::oj_dev(&app.0);
    assert_worker_stays_out_of_the_client(&dev);
}

#[test]
fn a_nested_server_routes_imports_stay_out_of_the_client_build() {
    let Some(app) = worker_routed("route-worker-build") else {
        return;
    };
    let (ok, text) = build(&app.0);
    assert!(ok, "the build failed:\n{text}");
    assert!(!text.contains("[import-protection]"), "{text}");
    for secret in ["worker-sync-secret", "worker-gate-secret"] {
        assert!(
            !client_has(&app.0, secret),
            "the client holds the worker route's server module ({secret})"
        );
    }
}

/// The worker route and its server modules written while the dev server
/// runs: the route set changes, the tree regenerates, and the new route is
/// as server-only as one there from the start.
#[tokio::test(flavor = "multi_thread")]
async fn a_server_route_added_in_dev_stays_out_of_the_client() {
    let Some(app) = file_routed("route-worker-added") else {
        return;
    };
    let dev = Server::oj_dev(&app.0);
    let (status, _) = dev.request("GET", "/@oj-start/client-entry.js", &[]);
    assert_eq!(status, 200, "the client entry\n{}", dev.output());
    let mark = dev.output().len();
    write_worker(&app.0);
    rebuilt_for(&dev, "jobs-worker.ts", mark).await;
    assert_worker_stays_out_of_the_client(&dev);
}

/// Routes whose `notFoundComponent` the code splitter splits off
/// (`route.tsx?tsr-split=notFoundComponent`), in the shapes an app writes
/// it: an imported component, one declared in the file, an inline arrow, the
/// same imported component as `errorComponent` too (a binding shared by two
/// split groups), and an import both the component and the not-found
/// component render (shared with the reference file). Each split module must
/// define what it exports: a dangling `export { SplitNotFoundComponent }`
/// fails the import-protection transform's parse of the module.
const NOT_FOUND: &str = r#"export function NotFound() {
  return <p id="not-found">nothing here</p>;
}

export function Frame({ children }: { children: React.ReactNode }) {
  return <section className="frame">{children}</section>;
}
"#;

const DOCS: &str = r#"import { createFileRoute } from "@tanstack/react-router";
import { NotFound } from "../lib/not-found";

export const Route = createFileRoute("/docs")({
  notFoundComponent: NotFound,
  component: () => <p id="docs">docs page</p>,
});
"#;

const GUIDE: &str = r#"import { createFileRoute } from "@tanstack/react-router";

export const Route = createFileRoute("/guide")({
  notFoundComponent: GuideMissing,
  component: () => <p id="guide">guide page</p>,
});

function GuideMissing() {
  return <p>no such guide</p>;
}
"#;

const FAQ: &str = r#"import { createFileRoute } from "@tanstack/react-router";

export const Route = createFileRoute("/faq")({
  notFoundComponent: () => <p>no such question</p>,
  component: () => <p id="faq">faq page</p>,
});
"#;

const BLOG: &str = r#"import { createFileRoute } from "@tanstack/react-router";
import { NotFound } from "../lib/not-found";

export const Route = createFileRoute("/blog")({
  errorComponent: NotFound,
  notFoundComponent: NotFound,
  component: () => <p id="blog">blog page</p>,
});
"#;

const SHOP: &str = r#"import { createFileRoute } from "@tanstack/react-router";
import { Frame, NotFound } from "../lib/not-found";

export const Route = createFileRoute("/shop")({
  notFoundComponent: () => (
    <Frame>
      <NotFound />
    </Frame>
  ),
  component: () => (
    <Frame>
      <p id="shop">shop page</p>
    </Frame>
  ),
});
"#;

const NOT_FOUND_ROUTES: [(&str, &str, &str); 5] = [
    ("docs.tsx", DOCS, "docs page"),
    ("guide.tsx", GUIDE, "guide page"),
    ("faq.tsx", FAQ, "faq page"),
    ("blog.tsx", BLOG, "blog page"),
    ("shop.tsx", SHOP, "shop page"),
];

fn not_found_routed(name: &str) -> Option<start_app::App> {
    let app = file_routed(name)?;
    let root = &app.0;
    std::fs::create_dir_all(root.join("src/lib")).unwrap();
    std::fs::write(root.join("src/lib/not-found.tsx"), NOT_FOUND).unwrap();
    for (file, code, _) in NOT_FOUND_ROUTES {
        std::fs::write(root.join("src/routes").join(file), code).unwrap();
    }
    Some(app)
}

#[tokio::test(flavor = "multi_thread")]
async fn routes_with_a_split_not_found_component_serve_in_dev() {
    let Some(app) = not_found_routed("route-not-found-dev") else {
        return;
    };
    let dev = Server::oj_dev(&app.0);
    for (file, _, text) in NOT_FOUND_ROUTES {
        let path = format!("/{}", file.trim_end_matches(".tsx"));
        let (status, body) = dev.request("GET", &path, &[]);
        assert!(
            status == 200 && body.contains(text),
            "{path} answered {status} {body:?}\n{}",
            dev.output()
        );
    }
    let (status, _) = dev.request("GET", "/@oj-start/client-entry.js", &[]);
    assert_eq!(status, 200, "the client entry\n{}", dev.output());
    let output = dev.output();
    assert!(
        !output.contains("is not defined") && !output.contains("[import-protection]"),
        "{output}"
    );
    if browser::skip() {
        return;
    }
    for (file, _, text) in NOT_FOUND_ROUTES {
        let path = format!("/{}", file.trim_end_matches(".tsx"));
        let page = Page::open(&dev.url(&path)).await;
        let rendered = page
            .poll(
                &format!(r##"document.body.textContent.includes("{text}")"##),
                Duration::from_secs(30),
            )
            .await;
        let errors = page.errors();
        page.close().await;
        assert!(
            rendered,
            "{path} did not render: {errors:?}\n{}",
            dev.output()
        );
        assert_eq!(errors, Vec::<String>::new(), "{path} page errors");
    }
}

#[test]
fn routes_with_a_split_not_found_component_build() {
    let Some(app) = not_found_routed("route-not-found-build") else {
        return;
    };
    let (ok, text) = build(&app.0);
    assert!(ok, "the build failed:\n{text}");
    assert!(
        !text.contains("is not defined") && !text.contains("[import-protection]"),
        "{text}"
    );
}

/// The worker route edited while a page is open: the server reloads it, the
/// client's tree still leaves it out, and import protection says nothing.
#[tokio::test(flavor = "multi_thread")]
async fn an_edit_to_a_server_route_with_a_page_open_stays_out_of_the_client() {
    let Some(app) = worker_routed("route-worker-edit") else {
        return;
    };
    if browser::skip() {
        return;
    }
    let dev = Server::oj_dev(&app.0);
    assert_worker_stays_out_of_the_client(&dev);
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
    let mark = dev.output().len();
    std::fs::write(
        app.0.join("src/routes/api/public/hooks/jobs-worker.ts"),
        WORKER.replace("${syncRows()}:${gate()}", "${syncRows()}:${gate()}:edited"),
    )
    .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let (status, body) = dev.request("POST", "/api/public/hooks/jobs-worker", &[]);
        if status == 200 && body.ends_with(":edited") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the edited worker route never answered: {status} {body:?}\n{}",
            dev.output()
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    tokio::time::sleep(Duration::from_secs(3)).await;
    let errors = page.errors();
    page.close().await;
    let after = dev.output()[mark..].to_string();
    assert!(
        !after.contains("[import-protection]"),
        "import protection saw the edited worker route\n{after}"
    );
    assert_eq!(errors, Vec::<String>::new(), "page errors after the edit");
    let (status, _) = dev.request("GET", "/@oj-start/client-entry.js", &[]);
    assert_eq!(
        status,
        200,
        "the client entry after the edit\n{}",
        dev.output()
    );
}
