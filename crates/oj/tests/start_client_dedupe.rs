// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A Start client bundle has one copy of a `resolve.dedupe` package: a
//! dependency with its own `node_modules/react` gets the app's React, in the
//! dev client bundle and in `oj build`'s. TanStack Start's plugin lists
//! `react`, `react-dom`, `@tanstack/react-start` and `@tanstack/react-router`
//! in every environment's `resolve.dedupe` (react-start's
//! `configEnvironment`), and Vite's tryNodeResolve resolves a deduped package
//! from the root for every importer, so Vite's client has one React. Two
//! copies of React in one page fail every hook the dependency calls ("Invalid
//! hook call").
//!
//! The nested copy here is a stand-in that re-exports the app's React with a
//! marker, so the page renders either way and the marker in the bundle tells
//! whether the nested file was bundled.

#[allow(dead_code)]
mod start_app;

use std::path::Path;

use start_app::{app, Server};

const ROOT: &str = r#"import { createRootRoute, HeadContent, Outlet, Scripts } from "@tanstack/react-router";

export const Route = createRootRoute({
  head: () => ({ meta: [{ title: "client dedupe" }] }),
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
import { Counter } from "counter-widget";

export const Route = createFileRoute("/")({
  component: () => <Counter />,
});
"#;

const ROUTER: &str = r#"import { createRouter } from "@tanstack/react-router";
import { routeTree } from "./routeTree.gen";

export function getRouter() {
  return createRouter({ routeTree });
}
"#;

/// A dependency that calls a hook, resolving `react` from its own directory.
const WIDGET: &str = r#"import * as React from "react";
import { jsx, nestedJsx } from "react/jsx-runtime";

export function Counter() {
  const [count] = React.useState(1);
  return jsx("span", { id: "counter", children: "counter-widget " + count + " " + (React.nestedCopy ?? nestedJsx ?? "app-react") });
}
"#;

/// The copy of React the dependency pins for itself.
const NESTED_REACT: &str = r#"module.exports = Object.assign({}, require("../../../react"), { nestedCopy: "nested-react-copy" });
"#;

/// Its JSX runtime, the import compiled JSX makes.
const NESTED_JSX_RUNTIME: &str = r#"module.exports = Object.assign({}, require("../../../react/jsx-runtime"), { nestedJsx: "nested-jsx-runtime-copy" });
"#;

/// The fixture app, file-routed, with `counter-widget` installed beside the
/// fixture's packages (linked entry by entry) and a React of its own under
/// `node_modules/counter-widget/node_modules/react`.
fn dedupe_app(name: &str) -> Option<start_app::App> {
    let app = app(name)?;
    let root = &app.0;
    let routes = root.join("src/routes");
    std::fs::remove_dir_all(&routes).unwrap();
    std::fs::create_dir_all(&routes).unwrap();
    std::fs::write(routes.join("__root.tsx"), ROOT).unwrap();
    std::fs::write(routes.join("index.tsx"), INDEX).unwrap();
    std::fs::write(root.join("src/routeTree.ts"), "export {};\n").unwrap();
    std::fs::write(root.join("src/router.tsx"), ROUTER).unwrap();

    let shared = std::fs::read_link(root.join("node_modules")).unwrap();
    std::fs::remove_file(root.join("node_modules")).unwrap();
    let modules = root.join("node_modules");
    std::fs::create_dir(&modules).unwrap();
    for entry in std::fs::read_dir(&shared).unwrap().flatten() {
        std::os::unix::fs::symlink(entry.path(), modules.join(entry.file_name())).unwrap();
    }
    let widget = modules.join("counter-widget");
    let nested = widget.join("node_modules/react");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(
        widget.join("package.json"),
        r#"{ "name": "counter-widget", "version": "1.0.0", "type": "module", "main": "index.js" }"#,
    )
    .unwrap();
    std::fs::write(widget.join("index.js"), WIDGET).unwrap();
    std::fs::write(
        nested.join("package.json"),
        r#"{ "name": "react", "version": "18.3.1", "main": "index.js" }"#,
    )
    .unwrap();
    std::fs::write(nested.join("index.js"), NESTED_REACT).unwrap();
    std::fs::write(nested.join("jsx-runtime.js"), NESTED_JSX_RUNTIME).unwrap();
    Some(app)
}

/// The dev client bundle: the entry and every chunk reachable from it.
fn client_bundle(dev: &Server) -> String {
    let mut text = String::new();
    let mut seen = vec!["client-entry.js".to_string()];
    let mut next = 0;
    while next < seen.len() {
        let (status, code) = dev.request("GET", &format!("/@oj-start/{}", seen[next]), &[]);
        assert_eq!(status, 200, "/@oj-start/{}\n{}", seen[next], dev.output());
        for chunk in code
            .split(['"', '\''])
            .filter_map(|s| s.strip_prefix("./"))
            .filter(|s| s.ends_with(".js") && !s.contains('/'))
        {
            if !seen.iter().any(|c| c == chunk) {
                seen.push(chunk.to_string());
            }
        }
        text.push_str(&code);
        next += 1;
    }
    text
}

/// Every `.js` file under `dir`, concatenated.
fn js_text(dir: &Path) -> String {
    let mut out = String::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "js") {
                out.push_str(&std::fs::read_to_string(&path).unwrap());
            }
        }
    }
    out
}

#[test]
fn a_dependencys_own_react_is_not_in_the_dev_client_bundle() {
    let Some(app) = dedupe_app("client-dedupe-dev") else {
        return;
    };
    let dev = Server::oj_dev(&app.0);
    let (status, _) = dev.request("GET", "/", &[]);
    assert_eq!(status, 200, "the page\n{}", dev.output());
    let bundle = client_bundle(&dev);
    assert!(
        bundle.contains("counter-widget "),
        "the dependency is in the client bundle\n{}",
        dev.output()
    );
    for nested in ["nested-react-copy", "nested-jsx-runtime-copy"] {
        assert!(
            !bundle.contains(nested),
            "the client bundle holds the dependency's own copy of React ({nested})\n{}",
            dev.output()
        );
    }
}

#[test]
fn a_dependencys_own_react_is_not_in_the_built_client() {
    let Some(app) = dedupe_app("client-dedupe-build") else {
        return;
    };
    start_app::build(&app.0);
    let client = js_text(&app.0.join("dist/client"));
    assert!(
        client.contains("counter-widget "),
        "the dependency is in the client build"
    );
    for nested in ["nested-react-copy", "nested-jsx-runtime-copy"] {
        assert!(
            !client.contains(nested),
            "the client build holds the dependency's own copy of React ({nested})"
        );
    }
}
