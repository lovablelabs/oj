// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A Start dev render has one `@tanstack/react-router` instance: the root
//! route's `<HeadContent />` reads the router through the same context
//! object Start's server package publishes with `RouterProvider`
//! (`@tanstack/react-start-server`'s StartServer.js). A second instance of
//! the package gives `useRouter` an empty context, HeadContent throws on it,
//! and every document 500s from the first request on.
//!
//! The layout that makes a second instance is a second copy of the package
//! installed under a Start package (a peer version skew leaves
//! `@tanstack/react-start-server/node_modules/@tanstack/react-router`): the
//! app's files resolve the router to the root's copy, the server package's
//! files to the one under them. Vite has one instance because the Start
//! plugin lists the router in the ssr environment's `resolve.dedupe`
//! (react-start's `configEnvironment`) and Vite resolves a deduped package
//! from the root for every importer (`tryNodeResolve`'s basedir, and
//! `fetchModule`'s for an externalized import); oj's SSR resolvers took the
//! top-level `resolve.dedupe` alone, which that plugin leaves empty, and
//! served the nested copy to the server package.
//!
//! Also covered, with the fixture's one copy: a cold boot, a hot update of a
//! file in the server graph, the boots after it (the last restoring the
//! cached client bundle), and the router package claimed by
//! `ssr.noExternal`, which versions it for every importer alike.

#[allow(dead_code)]
mod start_app;

use std::path::Path;
use std::time::{Duration, Instant};

use start_app::{app, Server, WAIT};

fn root_route(title: &str) -> String {
    format!(
        r#"import {{ createRootRoute, HeadContent, Outlet, Scripts }} from "@tanstack/react-router";

export const Route = createRootRoute({{
  head: () => ({{ meta: [{{ title: "{title}" }}] }}),
  component: RootComponent,
}});

function RootComponent() {{
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
}}
"#
    )
}

const INDEX: &str = r#"import { createFileRoute } from "@tanstack/react-router";

export const Route = createFileRoute("/")({
  component: () => <p id="home">home</p>,
});
"#;

const ROUTER: &str = r#"import { createRouter } from "@tanstack/react-router";
import { routeTree } from "./routeTree.gen";

export function getRouter() {
  return createRouter({ routeTree });
}
"#;

/// The base config plus the router package in `ssr.noExternal`: the
/// pipeline then evaluates it, as Vite's module runner would.
const NO_EXTERNAL_CONFIG: &str = r#"import base from "./vite.config";

export default { ...base, ssr: { noExternal: ["@tanstack/react-router"] } };
"#;

/// What react-router logs when `useRouter` finds no provider in its context:
/// the signature of a second instance of the package.
const NO_PROVIDER: &str = "useRouter must be used inside a <RouterProvider>";

/// The fixture app with a file-routed root that renders `<HeadContent />`
/// from a `head()` title.
fn identity_app(name: &str) -> Option<start_app::App> {
    let app = app(name)?;
    let root = &app.0;
    let routes = root.join("src/routes");
    std::fs::remove_dir_all(&routes).unwrap();
    std::fs::create_dir_all(&routes).unwrap();
    std::fs::write(routes.join("__root.tsx"), root_route("router identity v1")).unwrap();
    std::fs::write(routes.join("index.tsx"), INDEX).unwrap();
    std::fs::write(root.join("src/routeTree.ts"), "export {};\n").unwrap();
    std::fs::write(root.join("src/router.tsx"), ROUTER).unwrap();
    Some(app)
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let path = entry.path();
        let dest = to.join(entry.file_name());
        if path.is_dir() {
            copy_dir(&path, &dest);
        } else {
            std::fs::copy(&path, dest).unwrap();
        }
    }
}

/// [`identity_app`] with a second copy of the router package under Start's
/// server package, as a peer version skew installs one. The fixture's
/// node_modules is linked entry by entry; `@tanstack/react-start` and
/// `@tanstack/react-start-server` are copies (a link would resolve their
/// imports beside the fixture's own copies), the latter with
/// `node_modules/@tanstack/react-router` of its own.
fn nested_router_app(name: &str) -> Option<start_app::App> {
    let app = identity_app(name)?;
    let root = &app.0;
    let shared = std::fs::read_link(root.join("node_modules")).unwrap();
    std::fs::remove_file(root.join("node_modules")).unwrap();
    let modules = root.join("node_modules");
    std::fs::create_dir(&modules).unwrap();
    for entry in std::fs::read_dir(&shared).unwrap().flatten() {
        if entry.file_name() == "@tanstack" {
            continue;
        }
        std::os::unix::fs::symlink(entry.path(), modules.join(entry.file_name())).unwrap();
    }
    let tanstack = modules.join("@tanstack");
    std::fs::create_dir(&tanstack).unwrap();
    for entry in std::fs::read_dir(shared.join("@tanstack"))
        .unwrap()
        .flatten()
    {
        let name = entry.file_name();
        let target = entry.path().canonicalize().unwrap();
        if name == "react-start" || name == "react-start-server" {
            copy_dir(&target, &tanstack.join(&name));
        } else {
            std::os::unix::fs::symlink(target, tanstack.join(&name)).unwrap();
        }
    }
    copy_dir(
        &shared
            .join("@tanstack/react-router")
            .canonicalize()
            .unwrap(),
        &tanstack.join("react-start-server/node_modules/@tanstack/react-router"),
    );
    Some(app)
}

/// The document at `/`: its status and `<title>`.
fn document(dev: &Server) -> (u16, Option<String>) {
    let (status, html) = dev.request("GET", "/", &[]);
    let title = html
        .split_once("<title>")
        .and_then(|(_, rest)| rest.split_once("</title>"))
        .map(|(title, _)| title.to_string());
    (status, title)
}

/// The document renders `title` and nothing rendered outside the provider.
fn assert_renders(dev: &Server, title: &str, when: &str) {
    let (status, got) = document(dev);
    assert_eq!(
        status,
        200,
        "{when}: the document's status\n{}",
        dev.output()
    );
    assert_eq!(
        got.as_deref(),
        Some(title),
        "{when}: the document's title\n{}",
        dev.output()
    );
    assert!(
        !dev.output().contains(NO_PROVIDER),
        "{when}: HeadContent read a router context RouterProvider never set\n{}",
        dev.output()
    );
}

/// Whether `/` comes to render `title` within the wait (an edit lands
/// through the watcher, after the write returns).
fn renders_eventually(dev: &Server, title: &str) -> bool {
    let deadline = Instant::now() + WAIT;
    loop {
        let (status, got) = document(dev);
        if status == 200 && got.as_deref() == Some(title) {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// The root route's title changes: a hot update of a module in the server
/// graph, which re-evaluates the route against the router instance the
/// provider already holds.
fn edit_title(dev: &Server, app: &start_app::App, title: &str) {
    std::fs::write(app.0.join("src/routes/__root.tsx"), root_route(title)).unwrap();
    assert!(
        renders_eventually(dev, title),
        "the edited title never rendered\n{}",
        dev.output()
    );
    assert_renders(dev, title, "after the hot update");
}

#[test]
fn a_second_router_copy_under_the_start_server_package_is_not_the_renders_router() {
    let Some(app) = nested_router_app("ssr-router-identity-nested") else {
        return;
    };
    let dev = Server::oj_dev(&app.0);
    assert_renders(
        &dev,
        "router identity v1",
        "with a second router copy under the server package",
    );
    edit_title(&dev, &app, "router identity v2");
}

#[test]
fn head_content_renders_cold_after_a_hot_update_and_on_later_boots() {
    let Some(app) = identity_app("ssr-router-identity") else {
        return;
    };
    let root = &app.0;

    let dev = Server::oj_dev(root);
    assert_renders(&dev, "router identity v1", "cold boot");
    edit_title(&dev, &app, "router identity v2");
    drop(dev);

    // The boot after the edit, and one more with nothing changed since,
    // which restores the client bundle from the cache.
    let dev = Server::oj_dev(root);
    assert_renders(&dev, "router identity v2", "the boot after the edit");
    drop(dev);
    let dev = Server::oj_dev(root);
    assert_renders(&dev, "router identity v2", "the restored boot");
    assert!(
        dev.output().contains("client bundle restored from cache"),
        "an unchanged app rebuilt its client bundle\n{}",
        dev.output()
    );
}

#[test]
fn a_router_package_in_no_external_is_one_instance_for_the_app_and_start() {
    let Some(app) = identity_app("ssr-router-identity-no-external") else {
        return;
    };
    let root = &app.0;
    std::fs::write(root.join("vite.router.config.ts"), NO_EXTERNAL_CONFIG).unwrap();

    let dev = Server::oj_dev_with_args(root, &["--config", "vite.router.config.ts"], &[]);
    assert_renders(
        &dev,
        "router identity v1",
        "cold boot, router in noExternal",
    );
    edit_title(&dev, &app, "router identity v2");
}
