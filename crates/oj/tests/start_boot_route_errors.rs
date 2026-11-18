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

/// A file-routed app with `/` and `/about`.
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
        dev.logs("route tree generation failed; serving with the route tree as it is on disk until the route files change"),
        "oj dev, {name}: the boot does not say it went on without a new tree\n{}",
        dev.output()
    );
    assert_eq!(
        std::fs::read_to_string(&tree).unwrap(),
        before,
        "oj dev, {name}: the tree on disk"
    );

    // The fix is picked up by the server that booted over the error. Written
    // again on the way: a server answers before its watcher is armed.
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        mend(root);
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

/// The pids of `parent`'s `oj start-script` children.
fn script_children(parent: u32) -> Vec<String> {
    let out = Command::new("pgrep")
        .args(["-P", &parent.to_string(), "-f", "start-script"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .map(String::from)
        .collect()
}

// Only the generator's own verdict on the routes is survived. A generator
// that dies any other way (killed here, as the kernel kills one out of
// memory) still ends boot: the server would otherwise come up with no word
// of why its tree is behind.
#[test]
fn a_generator_that_is_killed_still_ends_boot() {
    let Some(app) = routed_app("killed") else {
        return;
    };
    let root = &app.0;
    let log = root.join("oj-dev.log");
    let out = std::fs::File::create(&log).unwrap();
    let mut dev = Command::new(env!("CARGO_BIN_EXE_oj"))
        .arg("dev")
        .arg(root)
        .args(["--port", &free_port().to_string()])
        .current_dir(root)
        .stdout(Stdio::from(out.try_clone().unwrap()))
        .stderr(Stdio::from(out))
        .spawn()
        .unwrap();
    let output = || std::fs::read_to_string(&log).unwrap_or_default();
    // The first script child is the route tree's.
    let deadline = Instant::now() + Duration::from_secs(90);
    let child = loop {
        if let Some(pid) = script_children(dev.id()).into_iter().next() {
            break pid;
        }
        if let Ok(Some(status)) = dev.try_wait() {
            panic!(
                "oj dev exited before it ran the generator: {status}\n{}",
                output()
            );
        }
        if Instant::now() > deadline {
            let _ = dev.kill();
            panic!("oj dev never ran the generator\n{}", output());
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    Command::new("kill").args(["-9", &child]).status().unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = dev.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = dev.kill();
            let _ = dev.wait();
            panic!(
                "oj dev stayed up over a generator that was killed\n{}",
                output()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(!status.success(), "oj dev exited cleanly\n{}", output());
    assert!(
        output().contains("route tree generation failed"),
        "oj dev does not say what ended it\n{}",
        output()
    );
    assert!(
        !output().contains("serving with the route tree as it is on disk"),
        "oj dev went on over a generator that was killed\n{}",
        output()
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
