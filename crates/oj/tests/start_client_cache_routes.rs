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
//! The same holds for the other inputs a restore cannot see in the closure:
//! the env client code inlines (`import.meta.env`, from the `.env` files in
//! `envDir` and the process vars `envPrefix` admits) and a config file given
//! by `--config`. Vite reads all of them anew on every start.
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
    boot_with(root, &[], &[])
}

/// [`boot`] with extra arguments and environment variables.
fn boot_with(root: &Path, args: &[&str], env: &[(&str, &str)]) -> (String, String) {
    let dev = Server::oj_dev_with_args(root, args, env);
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

/// The home route renders what the client sees of the env and the config.
const ENV_INDEX: &str = r#"import { createFileRoute } from "@tanstack/react-router";

export const Route = createFileRoute("/")({
  component: () => (
    <p id="seen">
      {[import.meta.env.FIXTURE_FILE, import.meta.env.FIXTURE_SHELL, __FIXTURE_DEFINE__].join("|")}
    </p>
  ),
});
"#;

/// An env var from a `.env` file in `envDir`, one from the shell admitted by
/// `envPrefix`, and a define in a `--config` file each change the client
/// bundle: a boot after any of them changed rebuilds it with the new value,
/// and a boot with nothing changed restores it.
#[test]
fn the_env_and_a_config_file_key_the_restored_client_bundle() {
    let Some(app) = file_routed("client-cache-env") else {
        return;
    };
    let root = &app.0;
    std::fs::write(root.join("src/routes/index.tsx"), ENV_INDEX).unwrap();
    // The config under a name the store's epoch does not list, given by
    // --config, with the env files in a directory of their own.
    let config = std::fs::read_to_string(root.join("vite.config.ts")).unwrap();
    std::fs::remove_file(root.join("vite.config.ts")).unwrap();
    let config = config.replacen("envPrefix: [", "envDir: \"env\",\n  envPrefix: [", 1);
    assert!(config.contains("envDir"), "{config}");
    std::fs::write(root.join("vite.app.config.ts"), &config).unwrap();
    std::fs::create_dir_all(root.join("env")).unwrap();
    std::fs::write(root.join("env/.env"), "FIXTURE_FILE=file-one\n").unwrap();
    let args = ["--config", "vite.app.config.ts"];
    let shell = |v: &'static str| [("FIXTURE_SHELL", v)];

    let (bundle, log) = boot_with(root, &args, &shell("shell-one"));
    assert!(
        bundle.contains("file-one")
            && bundle.contains("shell-one")
            && bundle.contains("fixture-define-marker"),
        "{log}"
    );

    let restored = |log: &str| log.contains("client bundle restored from cache");
    let (bundle, log) = boot_with(root, &args, &shell("shell-one"));
    assert!(
        restored(&log),
        "an unchanged app rebuilt its client bundle\n{log}"
    );
    assert!(bundle.contains("file-one"), "{log}");

    std::fs::write(root.join("env/.env"), "FIXTURE_FILE=file-two\n").unwrap();
    let (bundle, log) = boot_with(root, &args, &shell("shell-one"));
    assert!(
        !restored(&log) && bundle.contains("file-two") && !bundle.contains("file-one"),
        "an envDir file edit kept the old bundle\n{log}"
    );

    let (bundle, log) = boot_with(root, &args, &shell("shell-two"));
    assert!(
        !restored(&log) && bundle.contains("shell-two") && !bundle.contains("shell-one"),
        "a prefixed shell var kept the old bundle\n{log}"
    );

    std::fs::write(
        root.join("vite.app.config.ts"),
        config.replace("fixture-define-marker", "define-two"),
    )
    .unwrap();
    let (bundle, log) = boot_with(root, &args, &shell("shell-two"));
    assert!(
        !restored(&log)
            && bundle.contains("define-two")
            && !bundle.contains("fixture-define-marker"),
        "a --config file edit kept the old bundle\n{log}"
    );

    let (_, log) = boot_with(root, &args, &shell("shell-two"));
    assert!(
        restored(&log),
        "an unchanged app rebuilt its client bundle\n{log}"
    );
}
