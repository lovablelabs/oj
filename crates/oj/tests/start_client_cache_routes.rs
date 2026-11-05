// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The Start dev client bundle restored from oj's cache holds the routes the
//! app has now. The bundle loads its routes from the route tree, which the
//! dev server regenerates on every boot; the tree is in no closure (the dev
//! server rebundles on a route change itself), so the store keys each
//! generation by the trees' contents. A route created while no server ran
//! is in the client the next boot serves, and a boot with nothing changed
//! still restores. Before, the restore matched the old closure and served
//! the old tree: the new route answered on the server and 404'd in the
//! browser until `.oj-cache` was deleted.
//!
//! The app is the Start fixture (see `start_app`) moved to file-based
//! routing.

#[allow(dead_code)]
mod start_app;

use std::path::Path;

use start_app::{app, Server};

const ROOT: &str = r#"import { createRootRoute, HeadContent, Outlet, Scripts } from "@tanstack/react-router";

export const Route = createRootRoute({
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

const INDEX: &str = r#"import { createFileRoute } from "@tanstack/react-router";

export const Route = createFileRoute("/")({
  component: () => <p id="home">home page</p>,
});
"#;

const EXTRA: &str = r#"import { createFileRoute } from "@tanstack/react-router";

export const Route = createFileRoute("/extra")({
  component: () => <p id="extra">extra page</p>,
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
    std::fs::create_dir_all(&routes).unwrap();
    std::fs::write(routes.join("__root.tsx"), ROOT).unwrap();
    std::fs::write(routes.join("index.tsx"), INDEX).unwrap();
    std::fs::write(root.join("src/routeTree.ts"), "export {};\n").unwrap();
    std::fs::write(root.join("src/router.tsx"), ROUTER).unwrap();
    Some(app)
}

/// The client bundle the dev server serves: its entry and every chunk the
/// entry names, concatenated.
fn client_bundle(dev: &Server) -> String {
    let (status, entry) = dev.request("GET", "/@oj-start/client-entry.js", &[]);
    assert_eq!(status, 200, "the client entry\n{}", dev.output());
    let mut text = entry.clone();
    for chunk in entry
        .split(['"', '\''])
        .filter_map(|s| s.strip_prefix("./"))
        .filter(|s| s.ends_with(".js"))
    {
        let (_, code) = dev.request("GET", &format!("/@oj-start/{chunk}"), &[]);
        text.push_str(&code);
    }
    text
}

/// One `oj dev` boot of `root`: its client bundle and its log.
fn boot(root: &Path) -> (String, String) {
    let dev = Server::oj_dev(root);
    let (status, _) = dev.request("GET", "/", &[]);
    assert_eq!(status, 200, "the page\n{}", dev.output());
    let bundle = client_bundle(&dev);
    (bundle, dev.output())
}

#[test]
fn a_route_added_between_boots_is_in_the_restored_client_bundle() {
    let Some(app) = file_routed("client-cache-routes") else {
        return;
    };
    let root = &app.0;

    let (bundle, log) = boot(root);
    assert!(!bundle.contains("\"/extra\""), "{log}");
    assert!(log.contains("client bundle cached"), "{log}");

    // A route created while no server runs.
    std::fs::write(root.join("src/routes/extra.tsx"), EXTRA).unwrap();
    let (bundle, log) = boot(root);
    assert!(
        bundle.contains("\"/extra\""),
        "the client bundle lacks the new route\n{log}"
    );
    assert!(
        !log.contains("client bundle restored from cache"),
        "the old bundle was restored\n{log}"
    );

    // Nothing changed since: the bundle restores, the route still in it.
    let (bundle, log) = boot(root);
    assert!(
        log.contains("client bundle restored from cache"),
        "an unchanged app rebuilt its client bundle\n{log}"
    );
    assert!(bundle.contains("\"/extra\""), "{log}");
}
