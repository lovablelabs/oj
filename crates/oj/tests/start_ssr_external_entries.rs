// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The entry a Start server render loads for an external dependency. Vite
//! loads every external through Node's resolution (fetchModule's
//! tryNodeResolve: mainFields ["main"], the external conditions); oj prefers
//! the `module` entry, which a UMD `main` the CommonJS lexer cannot read
//! needs. A `module` build that calls `require` is bundler-only, though:
//! turndown's requires its Node DOM parser as it loads, and in an ES module
//! that throws "require is not defined" on every render. Such a dependency
//! gets Node's entry, as in Vite, whether the app imports it or another
//! dependency does; one whose `module` build declares its own `require`
//! (`createRequire`), or calls none, keeps the `module` entry.
//!
//! Each shape is its own page, loaded by its loader, so one page failing
//! leaves the others standing. The pages Vite can render are compared with
//! Vite's dev server on the same app.

#[allow(dead_code)]
mod start_app;

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use start_app::{app, build, free_port, Server};

const PACKAGES: &[(&str, &str)] = &[
    // turndown's shape: a CommonJS `main`, an ESM `module` calling `require`
    // as it loads.
    (
        "esm-requires/package.json",
        r#"{"name":"esm-requires","main":"lib/index.cjs.js","module":"lib/index.es.js"}"#,
    ),
    (
        "esm-requires/lib/index.cjs.js",
        "var parser = require(\"./parser.cjs\");\nexports.parse = function (s) { return \"main-entry:\" + parser.parse(s); };\n",
    ),
    (
        "esm-requires/lib/index.es.js",
        "function createParser() {\n  var parser = require(\"./parser.cjs\");\n  return parser;\n}\nvar parser = createParser();\nexport function parse(s) { return \"module-entry:\" + parser.parse(s); }\n",
    ),
    (
        "esm-requires/lib/parser.cjs",
        "exports.parse = function (s) { return s.toUpperCase(); };\n",
    ),
    // A dependency whose own ESM build imports it.
    (
        "wraps-esm-requires/package.json",
        r#"{"name":"wraps-esm-requires","main":"index.cjs","module":"index.mjs"}"#,
    ),
    (
        "wraps-esm-requires/index.cjs",
        "var dep = require(\"esm-requires\");\nexports.wrapped = function (s) { return \"wrapped:\" + dep.parse(s); };\n",
    ),
    (
        "wraps-esm-requires/index.mjs",
        "import { parse } from \"esm-requires\";\nexport function wrapped(s) { return \"wrapped:\" + parse(s); }\n",
    ),
    // detect-gpu's shape: a UMD `main` whose exports the lexer cannot see,
    // a plain ESM `module`.
    (
        "umd-main/package.json",
        r#"{"name":"umd-main","main":"umd.js","module":"esm.js"}"#,
    ),
    (
        "umd-main/umd.js",
        "(function (g, f) { typeof exports === \"object\" ? f(exports) : f((g.umdMain = {})); })(this, function (e) { e.named = function () { return \"umd-main-umd\"; }; });\n",
    ),
    (
        "umd-main/esm.js",
        "export function named() { return \"umd-main-module-entry\"; }\n",
    ),
    // An ESM `module` with a `require` of its own.
    (
        "creates-require/package.json",
        r#"{"name":"creates-require","main":"main.cjs","module":"esm.mjs"}"#,
    ),
    (
        "creates-require/main.cjs",
        "exports.which = function () { return \"creates-require-main\"; };\n",
    ),
    (
        "creates-require/esm.mjs",
        "import { createRequire } from \"node:module\";\nconst require = createRequire(import.meta.url);\nconst helper = require(\"./helper.cjs\");\nexport function which() { return \"creates-require-module:\" + helper.tag; }\n",
    ),
    (
        "creates-require/helper.cjs",
        "exports.tag = \"helper\";\n",
    ),
];

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

const ROUTER: &str = r#"import { createRouter } from "@tanstack/react-router";
import { routeTree } from "./routeTree.gen";

export function getRouter() {
  return createRouter({ routeTree });
}
"#;

/// Each page: its path, the module its loader imports, and that module.
const PAGES: &[(&str, &str, &str)] = &[
    (
        "app-import",
        "esm-requires",
        "import { parse } from \"esm-requires\";\nexport const run = () => parse(\"hi\");\n",
    ),
    (
        "dep-import",
        "wraps-esm-requires",
        "import { wrapped } from \"wraps-esm-requires\";\nexport const run = () => wrapped(\"hi\");\n",
    ),
    (
        "umd-main",
        "umd-main",
        "import { named } from \"umd-main\";\nexport const run = () => named();\n",
    ),
    (
        "creates-require",
        "creates-require",
        "import { which } from \"creates-require\";\nexport const run = () => which();\n",
    ),
];

fn page_route(path: &str) -> String {
    format!(
        r#"import {{ createFileRoute }} from "@tanstack/react-router";

export const Route = createFileRoute("/{path}")({{
  loader: async () => ({{ out: (await import("../lib/{path}")).run() }}),
  component: () => <p id="out">{{Route.useLoaderData().out}}</p>,
}});
"#
    )
}

/// The Start fixture with only the pages above, and the packages in a
/// node_modules of its own (the fixture's packages linked beside them).
fn externals_app(name: &str) -> Option<start_app::App> {
    let app = app(name)?;
    let root = &app.0;
    let shared = std::fs::read_link(root.join("node_modules")).unwrap();
    std::fs::remove_file(root.join("node_modules")).unwrap();
    std::fs::create_dir(root.join("node_modules")).unwrap();
    for entry in std::fs::read_dir(&shared).unwrap().flatten() {
        std::os::unix::fs::symlink(
            entry.path(),
            root.join("node_modules").join(entry.file_name()),
        )
        .unwrap();
    }
    for (file, body) in PACKAGES {
        let path = root.join("node_modules").join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }
    let routes = root.join("src/routes");
    std::fs::remove_dir_all(&routes).unwrap();
    std::fs::create_dir_all(&routes).unwrap();
    std::fs::write(routes.join("__root.tsx"), ROOT).unwrap();
    for (path, _, module) in PAGES {
        std::fs::write(routes.join(format!("{path}.tsx")), page_route(path)).unwrap();
        std::fs::write(root.join(format!("src/lib/{path}.ts")), module).unwrap();
    }
    std::fs::write(root.join("src/routeTree.ts"), "export {};\n").unwrap();
    std::fs::write(root.join("src/router.tsx"), ROUTER).unwrap();
    Some(app)
}

/// What a page rendered: its status and `#out` text, or its error.
fn rendered(html_of: &impl Fn(&str) -> (u16, String), page: &str) -> (u16, String) {
    let (status, html) = html_of(&format!("/{page}"));
    let out = html
        .split_once("<p id=\"out\">")
        .and_then(|(_, rest)| rest.split_once("</p>"))
        .map(|(text, _)| text.to_string());
    match out {
        Some(text) => (status, text),
        None => (status, html),
    }
}

/// The pages whose dependency calls `require` in its ESM build: Node's entry,
/// from the app and from another dependency.
fn assert_node_entries(html_of: &impl Fn(&str) -> (u16, String), label: &str) {
    assert_eq!(
        rendered(html_of, "app-import"),
        (200, "main-entry:HI".to_string()),
        "{label}: the app's import takes Node's entry"
    );
    assert_eq!(
        rendered(html_of, "dep-import"),
        (200, "wrapped:main-entry:HI".to_string()),
        "{label}: a dependency's import takes Node's entry"
    );
}

/// The pages whose ESM build runs as one: the `module` entry still.
fn assert_module_entries(html_of: &impl Fn(&str) -> (u16, String), label: &str) {
    assert_eq!(
        rendered(html_of, "umd-main"),
        (200, "umd-main-module-entry".to_string()),
        "{label}: a UMD main's package keeps its module entry"
    );
    assert_eq!(
        rendered(html_of, "creates-require"),
        (200, "creates-require-module:helper".to_string()),
        "{label}: a module build with its own require keeps it"
    );
}

/// Vite's dev server on `root`, once it answers.
struct ViteDev {
    child: Child,
    port: u16,
}

impl ViteDev {
    fn start(root: &Path) -> ViteDev {
        let port = free_port();
        let child = Command::new("node")
            .arg(root.join("node_modules/vite/bin/vite.js"))
            .args([
                "dev",
                "--host",
                "127.0.0.1",
                "--port",
                &port.to_string(),
                "--strictPort",
            ])
            .current_dir(root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(Instant::now() < deadline, "vite dev never listened");
            std::thread::sleep(Duration::from_millis(100));
        }
        ViteDev { child, port }
    }

    fn get(&self, path: &str) -> (u16, String) {
        let out = Command::new("curl")
            .args(["-s", "-m", "60", "-w", "\n%{http_code}"])
            .arg(format!("http://127.0.0.1:{}{path}", self.port))
            .output()
            .unwrap();
        let body = String::from_utf8_lossy(&out.stdout).into_owned();
        let (html, code) = body.rsplit_once('\n').unwrap_or((&body, "0"));
        (code.trim().parse().unwrap_or(0), html.to_string())
    }
}

impl Drop for ViteDev {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn dev_renders_each_external_from_the_entry_it_can_run() {
    let Some(app) = externals_app("ssr-external-entries-dev") else {
        return;
    };
    let root = &app.0;
    let dev = Server::oj_dev(root);
    let ours = |path: &str| dev.request("GET", path, &[]);
    assert_node_entries(&ours, "oj dev");
    assert_module_entries(&ours, "oj dev");
    let oj: Vec<_> = ["app-import", "dep-import"]
        .iter()
        .map(|page| rendered(&ours, page))
        .collect();
    drop(dev);

    let vite = ViteDev::start(root);
    let theirs = |path: &str| vite.get(path);
    assert_node_entries(&theirs, "vite dev");
    let vite_pages: Vec<_> = ["app-import", "dep-import"]
        .iter()
        .map(|page| rendered(&theirs, page))
        .collect();
    assert_eq!(oj, vite_pages, "oj dev vs vite dev");
}

#[test]
fn the_build_bundles_each_external() {
    let Some(app) = externals_app("ssr-external-entries-build") else {
        return;
    };
    let root = &app.0;
    build(root);
    let server = Server::node_server(root);
    let ours = |path: &str| server.request("GET", path, &[]);
    // The server bundle carries each dependency, a `require` in its ESM
    // build included, so every page renders the module build it bundled.
    for (page, want) in [
        ("app-import", "module-entry:HI"),
        ("dep-import", "wrapped:module-entry:HI"),
        ("umd-main", "umd-main-module-entry"),
    ] {
        assert_eq!(
            rendered(&ours, page),
            (200, want.to_string()),
            "oj build /{page}"
        );
    }
}
