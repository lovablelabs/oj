// SPDX-License-Identifier: MIT

//! `oj dev` on a TanStack Start app whose route tree cannot be generated at
//! boot stays up, as `vite dev` does: the router plugin logs the generator's
//! error (its `generate()` catches) and the server runs on the tree on disk.
//! oj's own boot generation ended the process ("route tree generation
//! failed"), so a dev server restarted over two route files claiming one
//! path, or over a route file that does not parse, never came back.
//!
//! Each boot runs under Vite first (the premise), then under oj.

#[allow(dead_code)]
mod start_app;

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use start_app::{app_from, free_port, Server};

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

const CONFIG: &str = r#"import { tanstackStart } from "@tanstack/react-start/plugin/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

export default defineConfig({ plugins: [tanstackStart(), react()] });
"#;

const PACKAGE: &str = r#"{ "name": "boot-route-errors", "private": true, "type": "module",
  "dependencies": { "@tanstack/react-router": "*", "@tanstack/react-start": "*", "react": "*", "react-dom": "*" },
  "devDependencies": { "@vitejs/plugin-react": "*", "vite": "*" } }
"#;

const TSCONFIG: &str = r#"{ "compilerOptions": { "jsx": "react-jsx", "module": "ESNext", "moduleResolution": "Bundler", "target": "ES2022", "strict": true, "skipLibCheck": true } }
"#;

/// A route file for `route` whose page shows `marker`.
fn route(route: &str, marker: &str) -> String {
    format!(
        r#"import {{ createFileRoute }} from "@tanstack/react-router";

export const Route = createFileRoute("{route}")({{ component: Page }});

function Page() {{
  return (
    <main>
      <h1>{marker}</h1>
    </main>
  );
}}
"#
    )
}

/// What the server-only route's handler answers with: never in a client
/// bundle.
const SERVER_ONLY: &str = "server-only-handler-marker";

/// The dev client's entry.
const CLIENT_ENTRY: &str = "/@oj-start/client-entry.js";

/// A file-routed app with `/`, `/about` and a route with only `server`
/// options, which the client's tree leaves out.
fn routed_app(name: &str) -> Option<start_app::App> {
    app_from(
        name,
        &[
            ("package.json", PACKAGE.to_string()),
            ("tsconfig.json", TSCONFIG.to_string()),
            ("vite.config.ts", CONFIG.to_string()),
            ("src/router.tsx", ROUTER.to_string()),
            ("src/routes/__root.tsx", ROOT.to_string()),
            ("src/routes/index.tsx", route("/", "home-marker")),
            ("src/routes/about.tsx", route("/about", "about-marker")),
            (
                "src/routes/api.ts",
                format!(
                    r#"import {{ createFileRoute }} from "@tanstack/react-router";

export const Route = createFileRoute("/api")({{
  server: {{ handlers: {{ GET: async () => new Response("{SERVER_ONLY}") }} }},
}});
"#
                ),
            ),
        ],
    )
}

/// Vite's dev server on `root`, once it listens; its output is kept.
struct ViteDev {
    child: Child,
    port: u16,
    log: std::path::PathBuf,
}

impl ViteDev {
    fn start(root: &Path) -> ViteDev {
        let port = free_port();
        let log = root.join("vite-dev.log");
        let out = std::fs::File::create(&log).unwrap();
        let child = Command::new("node")
            .arg(root.join("node_modules/vite/bin/vite.js"))
            .args(["dev", "--host", "127.0.0.1", "--strictPort", "--port"])
            .arg(port.to_string())
            .current_dir(root)
            .stdout(Stdio::from(out.try_clone().unwrap()))
            .stderr(Stdio::from(out))
            .spawn()
            .unwrap();
        let mut vite = ViteDev { child, port, log };
        let deadline = Instant::now() + Duration::from_secs(60);
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            if let Ok(Some(status)) = vite.child.try_wait() {
                panic!(
                    "vite dev exited before listening: {status}\n{}",
                    vite.output()
                );
            }
            assert!(Instant::now() < deadline, "vite dev never listened");
            std::thread::sleep(Duration::from_millis(100));
        }
        vite
    }

    fn output(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// The status `path` answers with.
    fn status(&self, path: &str) -> u16 {
        let out = Command::new("curl")
            .args(["-s", "-m", "60", "-o", "/dev/null", "-w", "%{http_code}"])
            .arg(format!("http://127.0.0.1:{}{path}", self.port))
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .unwrap_or(0)
    }
}

impl Drop for ViteDev {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `path` answers 200 with `marker` within a minute.
fn renders(dev: &Server, path: &str, marker: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (status, body) = dev.request("GET", path, &[]);
        if status == 200 && body.contains(marker) {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// oj's cache directory for the Start app at `root`.
fn start_cache(root: &Path) -> std::path::PathBuf {
    let cache = root.join(".oj-cache");
    let version = std::fs::read_dir(&cache)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|name| name.starts_with('v') && name[1..].chars().all(|c| c.is_ascii_digit()))
        .expect("a versioned cache directory");
    cache.join(version).join("start")
}

/// The client chunks oj has written that hold the server-only handler.
fn server_only_chunks(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut dirs = vec![start_cache(root).join("client-chunks")];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else if std::fs::read(&path)
                .is_ok_and(|b| String::from_utf8_lossy(&b).contains(SERVER_ONLY))
            {
                found.push(path.display().to_string());
            }
        }
    }
    found
}

/// `path` answers `status` within a minute; `nudge` runs between tries.
fn answers(dev: &Server, path: &str, status: u16, nudge: impl Fn()) -> bool {
    let deadline = Instant::now() + Duration::from_secs(60);
    while dev.request("GET", path, &[]).0 != status {
        if Instant::now() > deadline {
            return false;
        }
        nudge();
        std::thread::sleep(Duration::from_secs(1));
    }
    true
}

/// One broken boot over the tree a good run left: `break_it` breaks the
/// route files, `mend` fixes them, `error` is what the generator says, and
/// `want` is the status of `/` and `/about` while they are broken.
fn boots_over_a_tree_it_cannot_regenerate(
    name: &str,
    break_it: impl Fn(&Path),
    mend: impl Fn(&Path),
    error: &str,
    want: (u16, u16),
    mended: (&str, &str),
) {
    let Some(app) = routed_app(name) else {
        return;
    };
    let root = &app.0;
    let tree = root.join("src/routeTree.gen.ts");

    // A good run leaves its tree.
    let good = Server::oj_dev(root);
    assert!(
        renders(&good, "/about", "about-marker"),
        "the good boot never rendered\n{}",
        good.output()
    );
    drop(good);
    let before = std::fs::read_to_string(&tree).expect("the good boot left a route tree");

    break_it(root);

    // The premise: Vite stays up on the tree that is there.
    let mut vite = ViteDev::start(root);
    let theirs = (vite.status("/"), vite.status("/about"));
    assert_eq!(
        theirs,
        want,
        "vite dev, {name}: the test's premise\n{}",
        vite.output()
    );
    assert!(
        vite.output().contains(error),
        "vite dev, {name}: the generator's error is not in its output\n{}",
        vite.output()
    );
    assert!(
        matches!(vite.child.try_wait(), Ok(None)),
        "vite dev, {name}: exited"
    );
    drop(vite);
    assert_eq!(
        std::fs::read_to_string(&tree).unwrap(),
        before,
        "vite dev, {name}: the tree on disk"
    );

    // `oj_dev` panics, with the output, when the process exits before it
    // listens: what a fatal boot generation did.
    let dev = Server::oj_dev(root);
    let ours = (
        dev.request("GET", "/", &[]).0,
        dev.request("GET", "/about", &[]).0,
    );
    assert_eq!(ours, theirs, "oj dev vs vite dev, {name}\n{}", dev.output());
    assert!(
        dev.logs(error),
        "oj dev, {name}: the generator's error is not in its output\n{}",
        dev.output()
    );
    assert!(
        dev.logs("serving with the route tree as it is on disk until the route files change"),
        "oj dev, {name}: the boot does not say it went on without a new tree\n{}",
        dev.output()
    );
    assert_eq!(
        std::fs::read_to_string(&tree).unwrap(),
        before,
        "oj dev, {name}: the tree on disk"
    );

    // The client is refused, with what the generator said, in place of one
    // built on the tree left on disk (Start's client tree plugin fails the
    // tree's load under Vite while its generator has no crawl).
    assert!(
        answers(&dev, CLIENT_ENTRY, 404, || {}),
        "oj dev, {name}: the client entry is served over routes that made no tree\n{}",
        dev.output()
    );
    let refused =
        std::fs::read_to_string(start_cache(root).join("client-error.json")).unwrap_or_default();
    assert!(
        refused.contains(error),
        "oj dev, {name}: the refused client does not carry the generator's error: {refused}"
    );
    assert_eq!(
        server_only_chunks(root),
        Vec::<String>::new(),
        "oj dev, {name}: the server-only route reached the client bundle"
    );

    // The fix is picked up by the server that booted over the error. A
    // route file is written again on the way: a server answers before its
    // watcher is armed.
    let touch =
        || std::fs::write(root.join("src/routes/index.tsx"), route("/", "home-marker")).unwrap();
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        mend(root);
        touch();
        let (status, body) = dev.request("GET", mended.0, &[]);
        if status == 200 && body.contains(mended.1) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "oj dev, {name}: {} never rendered once the route files were fixed\n{}",
            mended.0,
            dev.output()
        );
        std::thread::sleep(Duration::from_secs(2));
    }
    assert_eq!(
        dev.request("GET", "/", &[]).0,
        200,
        "oj dev, {name}: / once the route files were fixed\n{}",
        dev.output()
    );
    // And the client is back.
    assert!(
        answers(&dev, CLIENT_ENTRY, 200, touch),
        "oj dev, {name}: the client stayed refused once the route files were fixed\n{}",
        dev.output()
    );
    assert_eq!(
        server_only_chunks(root),
        Vec::<String>::new(),
        "oj dev, {name}: the server-only route reached the client bundle"
    );
}

/// The `_auth/index.tsx` that claims "/" beside `index.tsx`.
fn add_conflict(root: &Path) {
    let dir = root.join("src/routes/_auth");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("index.tsx"), route("/_auth/", "conflict-marker")).unwrap();
}

// Routes that break while the server runs, after it has generated a tree:
// Vite's client keeps loading the tree (Start's client tree plugin builds
// it from the generator's last crawl) and no page is told anything. oj
// keeps the client it has, built on the client tree of its last generation:
// no refusal, and no server-only route in the bundle from falling back to
// the tree on disk.
#[test]
fn routes_that_break_while_the_server_runs_leave_the_client_as_it_is() {
    let Some(app) = routed_app("in-session") else {
        return;
    };
    let root = &app.0;
    let said = "Conflicting configuration paths were found";

    // The premise.
    let vite = ViteDev::start(root);
    assert_eq!(
        vite.status("/about"),
        200,
        "vite dev: the healthy app\n{}",
        vite.output()
    );
    assert_eq!(vite.status("/src/routeTree.gen.ts"), 200);
    add_conflict(root);
    let deadline = Instant::now() + Duration::from_secs(60);
    while !vite.output().contains(said) {
        assert!(
            Instant::now() < deadline,
            "vite dev never ran its generator on the conflict\n{}",
            vite.output()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(
        (vite.status("/about"), vite.status("/src/routeTree.gen.ts")),
        (200, 200),
        "vite dev: the test's premise\n{}",
        vite.output()
    );
    drop(vite);
    std::fs::remove_dir_all(root.join("src/routes/_auth")).unwrap();
    let _ = std::fs::remove_file(root.join("src/routeTree.gen.ts"));

    let dev = Server::oj_dev(root);
    assert!(renders(&dev, "/about", "about-marker"), "{}", dev.output());
    assert!(answers(&dev, CLIENT_ENTRY, 200, || {}), "{}", dev.output());
    // The client's tree of the good generation: the api route pruned.
    let client_tree = start_cache(root).join("route-tree.client.js");
    let pruned = std::fs::read_to_string(&client_tree).expect("the client's tree");
    assert!(
        pruned.contains("routes/about") && !pruned.contains("routes/api"),
        "the client's tree: {pruned}"
    );
    // Added again until the generator has spoken: a server answers before
    // its watcher is armed.
    let deadline = Instant::now() + Duration::from_secs(90);
    while !dev.output().contains(said) {
        assert!(
            Instant::now() < deadline,
            "oj dev never ran its generator on the conflict\n{}",
            dev.output()
        );
        let _ = std::fs::remove_dir_all(root.join("src/routes/_auth"));
        std::thread::sleep(Duration::from_millis(500));
        add_conflict(root);
        std::thread::sleep(Duration::from_secs(3));
    }
    // Whatever rebundle the batch asked for has had its time.
    std::thread::sleep(Duration::from_secs(5));
    assert_eq!(
        (
            dev.request("GET", "/about", &[]).0,
            dev.request("GET", CLIENT_ENTRY, &[]).0
        ),
        (200, 200),
        "oj dev: the page and its client over routes that broke while it ran\n{}",
        dev.output()
    );
    assert!(
        !start_cache(root).join("client-error.json").exists(),
        "oj dev refused the client over routes that broke while it ran\n{}",
        dev.output()
    );
    // The client's tree is still the last generation's: a bundle made now
    // does not fall back to the tree on disk, server-only routes and all.
    assert_eq!(
        std::fs::read_to_string(&client_tree).ok().as_deref(),
        Some(pruned.as_str()),
        "oj dev: the client's tree after a generation that failed while it ran"
    );
    assert_eq!(
        server_only_chunks(root),
        Vec::<String>::new(),
        "oj dev: the server-only route reached the client bundle"
    );
}

// A build of the app fails on its routes while a dev server runs on the
// same cache directory. What the generator told the build is the build's:
// the dev server's client is not refused over it, before or after the
// route file is fixed.
#[test]
fn a_build_that_fails_on_the_routes_does_not_refuse_a_running_dev_servers_client() {
    let Some(app) = routed_app("build-while-dev") else {
        return;
    };
    let root = &app.0;
    let about = root.join("src/routes/about.tsx");
    let dev = Server::oj_dev(root);
    assert!(renders(&dev, "/about", "about-marker"), "{}", dev.output());
    assert!(answers(&dev, CLIENT_ENTRY, 200, || {}), "{}", dev.output());

    // An edit of a route file's content runs no generator in the dev server.
    let twice = "import { useState } from \"react\";\nimport { useState } from \"react\";\n";
    std::fs::write(
        &about,
        format!("{twice}{}", route("/about", "about-marker")),
    )
    .unwrap();
    let build = Command::new(env!("CARGO_BIN_EXE_oj"))
        .arg("build")
        .arg(root)
        .current_dir(root)
        .output()
        .unwrap();
    let built = format!(
        "{}{}",
        String::from_utf8_lossy(&build.stdout),
        String::from_utf8_lossy(&build.stderr)
    );
    assert!(
        !build.status.success() && built.contains("Error transforming route file"),
        "oj build: expected the generator to refuse the routes\n{built}"
    );
    let left: Vec<String> = std::fs::read_dir(start_cache(root))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("route-tree-error"))
        .collect();
    assert_eq!(
        left,
        Vec::<String>::new(),
        "the build left its generator's error in the cache"
    );

    // Fixed: the page and its client are served, however often it is saved.
    let fix = || std::fs::write(&about, route("/about", "about-marker")).unwrap();
    fix();
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let page = dev.request("GET", "/about", &[]);
        let entry = dev.request("GET", CLIENT_ENTRY, &[]).0;
        let refused = start_cache(root).join("client-error.json").exists();
        if page.0 == 200 && page.1.contains("about-marker") && entry == 200 && !refused {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "oj dev: page {}, client entry {entry}, refused {refused} after the fix\n{}",
            page.0,
            dev.output()
        );
        fix();
        std::thread::sleep(Duration::from_secs(2));
    }
}

// `index.tsx` and `_auth/index.tsx` are both "/": the generator refuses, and
// the pages keep rendering from the tree of the last good run.
#[test]
fn two_route_files_on_one_path_leave_the_dev_server_up_on_the_last_tree() {
    boots_over_a_tree_it_cannot_regenerate(
        "conflict",
        |root| {
            let dir = root.join("src/routes/_auth");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("index.tsx"), route("/_auth/", "conflict-marker")).unwrap();
        },
        |root| {
            let _ = std::fs::remove_dir_all(root.join("src/routes/_auth"));
        },
        "Conflicting configuration paths were found",
        (200, 200),
        ("/about", "about-marker"),
    );
}

// An unclosed element in a route file: the generator cannot transform it, the
// pages answer 500 (the file does not compile either), and render again once
// it is fixed.
#[test]
fn a_route_file_that_does_not_parse_leaves_the_dev_server_up() {
    let about = |root: &Path| root.join("src/routes/about.tsx");
    boots_over_a_tree_it_cannot_regenerate(
        "syntax",
        |root| {
            let broken = route("/about", "about-marker").replace("<main>", "<main><div>");
            std::fs::write(about(root), broken).unwrap();
        },
        |root| std::fs::write(about(root), route("/about", "about-marker")).unwrap(),
        "Error transforming route file",
        (500, 500),
        ("/about", "about-marker"),
    );
}

// A route file added since the tree was written, with an import declared
// twice: the generator's transform refuses it, though the file parses. The
// tree does not name it, so the pages it names render; once the import is
// fixed the tree is generated again and the new page renders, though neither
// the set of route files nor any route's shape changed.
#[test]
fn a_fix_that_changes_no_route_shape_still_regenerates_the_tree() {
    let contact = |root: &Path| root.join("src/routes/contact.tsx");
    boots_over_a_tree_it_cannot_regenerate(
        "redeclared",
        |root| {
            let twice =
                "import { useState } from \"react\";\nimport { useState } from \"react\";\n";
            let body = format!("{twice}{}", route("/contact", "contact-marker"));
            std::fs::write(contact(root), body).unwrap();
        },
        |root| std::fs::write(contact(root), route("/contact", "contact-marker")).unwrap(),
        "Error transforming route file",
        (200, 200),
        ("/contact", "contact-marker"),
    );
}

// A `.jsx` route file is a route file to the generator too (its physical
// `getRouteNodes` takes ts, tsx, js and jsx): fixing one regenerates the tree.
#[test]
fn a_fixed_jsx_route_file_regenerates_the_tree() {
    let contact = |root: &Path| root.join("src/routes/contact.jsx");
    boots_over_a_tree_it_cannot_regenerate(
        "jsx",
        |root| {
            let twice =
                "import { useState } from \"react\";\nimport { useState } from \"react\";\n";
            let body = format!("{twice}{}", route("/contact", "contact-marker"));
            std::fs::write(contact(root), body).unwrap();
        },
        |root| std::fs::write(contact(root), route("/contact", "contact-marker")).unwrap(),
        "Error transforming route file",
        (200, 200),
        ("/contact", "contact-marker"),
    );
}

/// The app's `node_modules` as its own directory of links into the
/// fixture's, with `@tanstack/router-generator` replaced by a package that
/// throws when it is imported.
fn with_a_generator_that_cannot_load(root: &Path) {
    let modules = root.join("node_modules");
    let installed = std::fs::read_link(&modules).unwrap();
    std::fs::remove_file(&modules).unwrap();
    let link_all = |from: &Path, to: &Path, skip: &str| {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap().flatten() {
            if entry.file_name() != skip {
                std::os::unix::fs::symlink(entry.path(), to.join(entry.file_name())).unwrap();
            }
        }
    };
    link_all(&installed, &modules, "@tanstack");
    link_all(
        &installed.join("@tanstack"),
        &modules.join("@tanstack"),
        "router-generator",
    );
    let stub = modules.join("@tanstack/router-generator");
    std::fs::create_dir_all(&stub).unwrap();
    std::fs::write(
        stub.join("package.json"),
        r#"{ "name": "@tanstack/router-generator", "version": "0.0.0", "type": "module", "exports": { ".": "./index.js", "./package.json": "./package.json" } }"#,
    )
    .unwrap();
    std::fs::write(
        stub.join("index.js"),
        "throw new Error(\"stub-generator-cannot-load\");\n",
    )
    .unwrap();
}

// Only the generator's own verdict on the routes is survived. A generator
// script that fails any other way (here its package throws on import; the
// same path as one killed for memory) still ends boot: the server would
// otherwise come up with no word of why its tree is behind.
#[test]
fn a_generator_that_fails_outside_its_verdict_still_ends_boot() {
    let Some(app) = routed_app("no-verdict") else {
        return;
    };
    let root = &app.0;
    with_a_generator_that_cannot_load(root);
    let dev = Command::new(env!("CARGO_BIN_EXE_oj"))
        .arg("dev")
        .arg(root)
        .args(["--port", &free_port().to_string()])
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = dev.id();
    // A server that stayed up is stopped, so the wait below ends.
    let watchdog = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(120));
        let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
    });
    let out = dev.wait_with_output().unwrap();
    drop(watchdog);
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.code().is_some_and(|code| code != 0),
        "oj dev did not end on its own with a failure: {}\n{output}",
        out.status
    );
    assert!(
        output.contains("stub-generator-cannot-load")
            && output.contains("route tree generation failed"),
        "oj dev does not say what ended it\n{output}"
    );
    assert!(
        !output.contains("serving with the route tree as it is on disk"),
        "oj dev went on over a generator that gave no verdict\n{output}"
    );
}

// A build has no server to keep up: it fails on routes that cannot make a
// tree, under Vite and under oj.
#[test]
fn a_build_fails_on_routes_that_cannot_make_a_tree() {
    let Some(app) = routed_app("build-conflict") else {
        return;
    };
    let root = &app.0;
    let dir = root.join("src/routes/_auth");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("index.tsx"), route("/_auth/", "conflict-marker")).unwrap();
    let said = "Conflicting configuration paths were found";

    let vite = Command::new("node")
        .arg(root.join("node_modules/vite/bin/vite.js"))
        .arg("build")
        .current_dir(root)
        .output()
        .unwrap();
    let theirs = format!(
        "{}{}",
        String::from_utf8_lossy(&vite.stdout),
        String::from_utf8_lossy(&vite.stderr)
    );
    assert!(
        !vite.status.success() && theirs.contains(said),
        "vite build: the test's premise\n{theirs}"
    );
    let _ = std::fs::remove_dir_all(root.join("dist"));

    let oj = Command::new(env!("CARGO_BIN_EXE_oj"))
        .arg("build")
        .arg(root)
        .current_dir(root)
        .output()
        .unwrap();
    let ours = format!(
        "{}{}",
        String::from_utf8_lossy(&oj.stdout),
        String::from_utf8_lossy(&oj.stderr)
    );
    assert!(
        !oj.status.success() && ours.contains(said),
        "oj build went on over routes that cannot make a tree\n{ours}"
    );
}
