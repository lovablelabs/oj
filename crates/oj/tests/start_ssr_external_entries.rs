// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The entry a Start server render loads for an external dependency. Vite
//! loads every external as fetchModule resolves it (tryNodeResolve:
//! mainFields ["main"], the external conditions); oj prefers the `module`
//! entry, which a UMD `main` the CommonJS lexer cannot read needs. A
//! `module` build that calls `require` is bundler-only, though: turndown's
//! requires its Node DOM parser as it loads, and in an ES module that throws
//! "require is not defined" on every render. Such a dependency gets the file
//! Vite loads: imported by the app or by another dependency, a nested copy,
//! a package subdirectory, esbuild's `__require` shim. One whose `module`
//! build declares its own `require` (`createRequire`), or calls none, keeps
//! the `module` entry, where Vite loads `main`: oj's deliberate divergence.
//!
//! Each shape is its own page, loaded by its loader, so one page failing
//! leaves the others standing. Every page that Vite renders is checked
//! against Vite's dev server on the same app.

#[allow(dead_code)]
mod start_app;

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use start_app::{app, build, free_port, Server};

/// A `module` build calling `require` as it loads, and its CommonJS `main`,
/// each tagging what it returns.
fn requiring_builds(dir: &str, tag: &str) -> Vec<(String, String)> {
    vec![
        (
            format!("{dir}/index.cjs.js"),
            format!("var parser = require(\"./parser.cjs\");\nexports.parse = function (s) {{ return \"{tag}main-entry:\" + parser.parse(s); }};\n"),
        ),
        (
            format!("{dir}/index.es.js"),
            format!("function createParser() {{\n  var parser = require(\"./parser.cjs\");\n  return parser;\n}}\nvar parser = createParser();\nexport function parse(s) {{ return \"{tag}module-entry:\" + parser.parse(s); }}\n"),
        ),
        (
            format!("{dir}/parser.cjs"),
            "exports.parse = function (s) { return s.toUpperCase(); };\n".to_string(),
        ),
    ]
}

fn packages() -> Vec<(String, String)> {
    let mut files: Vec<(String, String)> = [
        // turndown's shape.
        (
            "esm-requires/package.json",
            r#"{"name":"esm-requires","main":"index.cjs.js","module":"index.es.js"}"#,
        ),
        // A dependency whose own builds import it, each tagging itself.
        (
            "wraps-esm-requires/package.json",
            r#"{"name":"wraps-esm-requires","main":"index.cjs","module":"index.mjs"}"#,
        ),
        (
            "wraps-esm-requires/index.cjs",
            "var dep = require(\"esm-requires\");\nexports.wrapped = function (s) { return \"wrapped-main:\" + dep.parse(s); };\n",
        ),
        (
            "wraps-esm-requires/index.mjs",
            "import { parse } from \"esm-requires\";\nexport function wrapped(s) { return \"wrapped-module:\" + parse(s); }\n",
        ),
        // A dependency with a nested copy of it.
        (
            "has-nested/package.json",
            r#"{"name":"has-nested","main":"index.cjs","module":"index.mjs"}"#,
        ),
        (
            "has-nested/index.cjs",
            "var dep = require(\"esm-requires\");\nexports.wrapped = function (s) { return \"nested-main:\" + dep.parse(s); };\n",
        ),
        (
            "has-nested/index.mjs",
            "import { parse } from \"esm-requires\";\nexport function wrapped(s) { return \"nested-module:\" + parse(s); }\n",
        ),
        (
            "has-nested/node_modules/esm-requires/package.json",
            r#"{"name":"esm-requires","main":"index.cjs.js","module":"index.es.js"}"#,
        ),
        // The shape in a package subdirectory, imported as `deep-requires/sub`.
        ("deep-requires/package.json", r#"{"name":"deep-requires"}"#),
        (
            "deep-requires/sub/package.json",
            r#"{"main":"index.cjs.js","module":"index.es.js"}"#,
        ),
        // esbuild's `__require` shim in the `module` build.
        (
            "shim-requires/package.json",
            r#"{"name":"shim-requires","main":"index.cjs","module":"index.mjs"}"#,
        ),
        (
            "shim-requires/index.cjs",
            "exports.parse = function (s) { return \"shim-main-entry:\" + require(\"./parser.cjs\").parse(s); };\n",
        ),
        (
            "shim-requires/index.mjs",
            "var __require = /* @__PURE__ */ ((x) => typeof require !== \"undefined\" ? require : typeof Proxy !== \"undefined\" ? new Proxy(x, { get: (a, b) => (typeof require !== \"undefined\" ? require : a)[b] }) : x)(function (x) {\n  if (typeof require !== \"undefined\") return require.apply(this, arguments);\n  throw Error('Dynamic require of \"' + x + '\" is not supported');\n});\nvar parser = __require(\"./parser.cjs\");\nexport function parse(s) { return \"shim-module-entry:\" + parser.parse(s); }\n",
        ),
        (
            "shim-requires/parser.cjs",
            "exports.parse = function (s) { return s.toUpperCase(); };\n",
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
        ("creates-require/helper.cjs", "exports.tag = \"helper\";\n"),
    ]
    .map(|(f, b)| (f.to_string(), b.to_string()))
    .to_vec();
    files.extend(requiring_builds("esm-requires", ""));
    files.extend(requiring_builds(
        "has-nested/node_modules/esm-requires",
        "nested-copy-",
    ));
    files.extend(requiring_builds("deep-requires/sub", "deep-"));
    files
}

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

/// Each page: its path and the module its loader imports.
const PAGES: &[(&str, &str)] = &[
    (
        "app-import",
        "import { parse } from \"esm-requires\";\nexport const run = () => parse(\"hi\");\n",
    ),
    (
        "dep-import",
        "import { wrapped } from \"wraps-esm-requires\";\nexport const run = () => wrapped(\"hi\");\n",
    ),
    (
        "nested-copy",
        "import { wrapped } from \"has-nested\";\nexport const run = () => wrapped(\"hi\");\n",
    ),
    (
        "subdirectory",
        "import { parse } from \"deep-requires/sub\";\nexport const run = () => parse(\"hi\");\n",
    ),
    (
        "require-shim",
        "import { parse } from \"shim-requires\";\nexport const run = () => parse(\"hi\");\n",
    ),
    (
        "umd-main",
        "import { named } from \"umd-main\";\nexport const run = () => named();\n",
    ),
    (
        "creates-require",
        "import { which } from \"creates-require\";\nexport const run = () => which();\n",
    ),
];

/// What each page renders in dev on oj and on Vite. The dependency a page's
/// package imports is the file Vite loads on both; the importing package's
/// own entry is its `module` on oj, its `main` on Vite. The last two are
/// oj's alone: Vite cannot link the UMD's named export, and takes the main
/// of a `module` build that runs.
const DEV: &[(&str, &str, Option<&str>)] = &[
    ("app-import", "main-entry:HI", Some("main-entry:HI")),
    (
        "dep-import",
        "wrapped-module:main-entry:HI",
        Some("wrapped-main:main-entry:HI"),
    ),
    (
        "nested-copy",
        "nested-module:nested-copy-main-entry:HI",
        Some("nested-main:nested-copy-main-entry:HI"),
    ),
    (
        "subdirectory",
        "deep-main-entry:HI",
        Some("deep-main-entry:HI"),
    ),
    (
        "require-shim",
        "shim-main-entry:HI",
        Some("shim-main-entry:HI"),
    ),
    ("umd-main", "umd-main-module-entry", None),
    ("creates-require", "creates-require-module:helper", None),
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
    for (file, body) in packages() {
        let path = root.join("node_modules").join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }
    let routes = root.join("src/routes");
    std::fs::remove_dir_all(&routes).unwrap();
    std::fs::create_dir_all(&routes).unwrap();
    std::fs::write(routes.join("__root.tsx"), ROOT).unwrap();
    for (path, module) in PAGES {
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
fn dev_renders_each_external_from_the_entry_vite_loads() {
    let Some(app) = externals_app("ssr-external-entries-dev") else {
        return;
    };
    let root = &app.0;
    let dev = Server::oj_dev(root);
    let ours = |path: &str| dev.request("GET", path, &[]);
    for (page, want, _) in DEV {
        assert_eq!(
            rendered(&ours, page),
            (200, want.to_string()),
            "oj dev /{page}"
        );
    }
    drop(dev);

    let vite = ViteDev::start(root);
    let theirs = |path: &str| vite.get(path);
    for (page, _, want) in DEV {
        if let Some(want) = want {
            assert_eq!(
                rendered(&theirs, page),
                (200, want.to_string()),
                "vite dev /{page}"
            );
        }
    }
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
        ("dep-import", "wrapped-module:module-entry:HI"),
        ("nested-copy", "nested-module:nested-copy-module-entry:HI"),
        ("subdirectory", "deep-module-entry:HI"),
        ("umd-main", "umd-main-module-entry"),
    ] {
        assert_eq!(
            rendered(&ours, page),
            (200, want.to_string()),
            "oj build /{page}"
        );
    }
}
