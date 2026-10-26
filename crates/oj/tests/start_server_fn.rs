// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! TanStack Start server functions end to end on the `oj` binary, in dev and
//! in a build: a function behind a middleware that imports
//! `@tanstack/react-start/server`. That import reaches Start's handler, which
//! imports `#tanstack-start-server-fn-resolver`; a resolver that imported
//! every `*.functions.ts` statically ran the function module inside that
//! import, while its middleware module was still mid-evaluation: `Cannot
//! access 'authed' before initialization` and every SSR request 500. Start's
//! own resolver loads each module on the first call. The SSR entry imports
//! the middleware first, which fixes the order (a real app hit it at random,
//! by which module the server graph reached first).
//!
//! Each test runs a private copy of e2e/fixtures/start-app, which shares the
//! fixture's installed `node_modules`. The fixture is installed by
//! `npm ci --prefix e2e/fixtures/start-app`; without it the tests skip, unless
//! `OJ_REQUIRE_FIXTURES` is set. The browser half skips without Chrome,
//! unless `OJ_REQUIRE_BROWSER` is set.

#[path = "../../oj_server/tests/dev/browser.rs"]
#[allow(dead_code)]
mod browser;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use browser::Page;

const WAIT: Duration = Duration::from_secs(120);

/// A private copy of the fixture's sources, removed on drop (a failing
/// assert included).
struct App(PathBuf);

impl Drop for App {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The app, or None (after saying why) when the fixture is not installed.
fn app(name: &str) -> Option<App> {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../e2e/fixtures/start-app")
        .canonicalize()
        .unwrap();
    if !fixture.join("node_modules/@tanstack/react-start").is_dir() {
        assert!(
            std::env::var_os("OJ_REQUIRE_FIXTURES").is_none(),
            "OJ_REQUIRE_FIXTURES is set but the start-app fixture is not installed"
        );
        eprintln!("skipping: npm ci --prefix e2e/fixtures/start-app");
        return None;
    }
    let root = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("oj-start-server-fn-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    copy_sources(&fixture, &root);
    std::os::unix::fs::symlink(fixture.join("node_modules"), root.join("node_modules")).unwrap();
    Some(App(root))
}

/// Everything but installs and outputs: no node_modules, dist, caches, or a
/// route tree an earlier run generated.
fn copy_sources(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let name = entry.file_name();
        if matches!(
            name.to_str(),
            Some("node_modules" | "dist" | ".oj-cache" | ".tanstack" | "routeTree.gen.ts")
        ) {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            copy_sources(&path, &to.join(&name));
        } else {
            std::fs::copy(&path, to.join(&name)).unwrap();
        }
    }
}

/// The middleware, the function behind it, an SSR entry that imports the
/// middleware before anything else, and the about page calling the function
/// once it hydrates.
fn write_guarded(root: &Path) {
    std::fs::write(
        root.join("src/server/auth.ts"),
        r#"import { createMiddleware } from "@tanstack/react-start";
import { getRequest } from "@tanstack/react-start/server";

export const authed = createMiddleware({ type: "function" }).server(({ next }) =>
  next({ context: { method: getRequest()?.method ?? "none" } }),
);
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("src/server/guarded.functions.ts"),
        r#"import { createServerFn } from "@tanstack/react-start";
import { authed } from "./auth";

export const guarded = createServerFn({ method: "POST" })
  .middleware([authed])
  .handler(async ({ context }) => `guarded-${context.method}`);
"#,
    )
    .unwrap();
    let entry = root.join("src/ssr-entry.ts");
    let source = std::fs::read_to_string(&entry).unwrap();
    std::fs::write(&entry, format!("import \"./server/auth\";\n{source}")).unwrap();

    let about = root.join("src/routes/about.tsx");
    let source = std::fs::read_to_string(&about).unwrap();
    let import = "import { rootRoute } from \"./__root\";";
    let heading = "<h1 className=\"fixture-heading\">about-page-marker</h1>";
    assert!(
        source.contains(import) && source.contains(heading),
        "{source}"
    );
    let source = source
        .replacen(
            import,
            &format!(
                r#"{import}
import {{ useEffect, useState }} from "react";
import {{ guarded }} from "../server/guarded.functions";
function Guarded() {{
  const [out, setOut] = useState("pending");
  useEffect(() => {{
    guarded().then((r) => setOut(r), (e) => setOut("error " + String(e)));
  }}, []);
  return <p id="guarded">{{out}}</p>;
}}"#
            ),
            1,
        )
        .replacen(heading, &format!("{heading}\n      <Guarded />"), 1);
    std::fs::write(&about, source).unwrap();
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A server process, stopped on drop, whose stdout and stderr are kept
/// together: either may carry an SSR error.
struct Server {
    child: Child,
    port: u16,
    output: Arc<Mutex<String>>,
}

impl Server {
    /// `oj dev <root>`, on the port it reports: oj moves to the next port
    /// when the one asked for was taken in the meantime.
    fn oj_dev(root: &Path) -> Server {
        let mut server = Server::spawn(
            Command::new(env!("CARGO_BIN_EXE_oj"))
                .arg("dev")
                .arg(root)
                .args(["--port", &free_port().to_string()])
                .current_dir(root),
        );
        let deadline = Instant::now() + WAIT;
        server.port = loop {
            if let Some(port) = listening_port(&server.output()) {
                break port;
            }
            server.assert_alive("oj dev", deadline);
            std::thread::sleep(Duration::from_millis(50));
        };
        server
    }

    /// `node dist/server.mjs` in `root`, once it listens.
    fn node_server(root: &Path) -> Server {
        let port = free_port();
        let mut server = Server::spawn(
            Command::new("node")
                .arg(root.join("dist/server.mjs"))
                .env("PORT", port.to_string())
                .current_dir(root),
        );
        server.port = port;
        let deadline = Instant::now() + WAIT;
        while TcpStream::connect(("127.0.0.1", port)).is_err() {
            server.assert_alive("node dist/server.mjs", deadline);
            std::thread::sleep(Duration::from_millis(50));
        }
        server
    }

    fn spawn(cmd: &mut Command) -> Server {
        let mut child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let output = Arc::new(Mutex::new(String::new()));
        let stdout = child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>);
        let stderr = child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>);
        for stream in [stdout, stderr].into_iter().flatten() {
            let output = Arc::clone(&output);
            std::thread::spawn(move || {
                for line in BufReader::new(stream).lines().map_while(Result::ok) {
                    let mut out = output.lock().unwrap();
                    out.push_str(&line);
                    out.push('\n');
                }
            });
        }
        Server {
            child,
            port: 0,
            output,
        }
    }

    /// Panics (with the output) if the process exited or `deadline` passed
    /// before it was ready.
    fn assert_alive(&mut self, what: &str, deadline: Instant) {
        if let Ok(Some(status)) = self.child.try_wait() {
            panic!(
                "{what} exited before listening: {status}\n{}",
                self.output()
            );
        }
        assert!(
            Instant::now() < deadline,
            "{what} never listened\n{}",
            self.output()
        );
    }

    fn output(&self) -> String {
        self.output.lock().unwrap().clone()
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    /// Whether the output shows `needle` within a few seconds (a server logs
    /// an error after it answered).
    fn logs(&self, needle: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.output().contains(needle) {
            if Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        true
    }

    /// One HTTP/1.0 request: the status and the body. A connection refused
    /// or reset is retried until `WAIT` (a dev server can restart once early
    /// on), with the server's output in the panic if it never answers.
    fn request(&self, method: &str, path: &str, headers: &[(&str, String)]) -> (u16, String) {
        let mut head = format!(
            "{method} {path} HTTP/1.0\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\nContent-Length: 0\r\n",
            self.port
        );
        for (name, value) in headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        let deadline = Instant::now() + WAIT;
        let response = loop {
            let attempt = (|| {
                let mut stream = TcpStream::connect(("127.0.0.1", self.port))?;
                stream.set_read_timeout(Some(WAIT))?;
                stream.write_all(head.as_bytes())?;
                let mut bytes = Vec::new();
                stream.read_to_end(&mut bytes)?;
                Ok::<_, std::io::Error>(bytes)
            })();
            match attempt {
                Ok(bytes) if !bytes.is_empty() => {
                    break String::from_utf8_lossy(&bytes).into_owned()
                }
                result => assert!(
                    Instant::now() < deadline,
                    "{method} {path} got no response ({result:?})\n{}",
                    self.output()
                ),
            }
            std::thread::sleep(Duration::from_millis(200));
        };
        let status = response
            .split(' ')
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let body = response
            .split_once("\r\n\r\n")
            .map(|(_, b)| b.to_string())
            .unwrap_or_default();
        (status, body)
    }

    /// A server-function call the way Start's client makes it: a
    /// same-origin POST flagged `x-tsr-serverFn`.
    fn call(&self, id: &str) -> (u16, String) {
        self.request(
            "POST",
            &format!("/_serverFn/{id}"),
            &[("Origin", self.url("")), ("x-tsr-serverFn", "true".into())],
        )
    }
}

impl Drop for Server {
    /// SIGTERM first: oj stops the children it spawned (Start's bundler)
    /// on it, where a SIGKILL would orphan them.
    fn drop(&mut self) {
        // SAFETY: a signal to the child this test spawned and still owns.
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The port in oj's `http://localhost:<port>/` banner line.
fn listening_port(output: &str) -> Option<u16> {
    let rest = &output[output.find("http://localhost:")? + "http://localhost:".len()..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// The page renders, the function runs behind its middleware (over HTTP and
/// from the hydrated page), an id naming an `Object.prototype` member fails
/// as unknown, and no module ran before its import.
async fn renders_and_calls(label: &str, server: &Server) {
    let (status, body) = server.request("GET", "/about", &[]);
    assert!(
        status == 200 && body.contains("about-page-marker"),
        "{label}: /about answered {status}, want the rendered page\n{}\n{}",
        &body[..body.len().min(500)],
        server.output()
    );
    let id = oj_compiler::server_fns::server_fn_id("src/server/guarded.functions.ts", "guarded");
    let (status, body) = server.call(&id);
    assert!(
        status == 200 && body.contains("guarded-POST"),
        "{label}: the server fn answered {status} {body:?}, want guarded-POST\n{}",
        server.output()
    );
    if !browser::skip() {
        let page = Page::open(&server.url("/about")).await;
        let ran = page
            .poll(
                "document.querySelector('#guarded')?.textContent === 'guarded-POST'",
                WAIT,
            )
            .await;
        let shown: Option<String> = page
            .eval("document.querySelector('#guarded')?.textContent ?? null")
            .await;
        assert!(
            ran,
            "{label}: the page's call answered {shown:?}, want guarded-POST; page errors: {:?}\n{}",
            page.errors(),
            server.output()
        );
        assert_eq!(page.errors(), Vec::<String>::new(), "{label}: page errors");
        page.close().await;
    }
    let (status, _) = server.call("constructor");
    assert_eq!(status, 500, "{label}: /_serverFn/constructor");
    assert!(
        server.logs("Server function info not found for constructor"),
        "{label}: /_serverFn/constructor did not fail as an unknown id\n{}",
        server.output()
    );
    assert!(
        !server.output().contains("before initialization"),
        "{label}: a module ran before its import\n{}",
        server.output()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_fn_whose_middleware_imports_starts_server_renders_and_runs() {
    let Some(app) = app("cycle") else {
        return;
    };
    let root = &app.0;
    write_guarded(root);

    let dev = Server::oj_dev(root);
    renders_and_calls("dev", &dev).await;
    drop(dev);

    // The build on its own: none of the dev run's cached codegen.
    let _ = std::fs::remove_dir_all(root.join(".oj-cache"));
    let out = Command::new(env!("CARGO_BIN_EXE_oj"))
        .arg("build")
        .arg(root)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "oj build failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let built = Server::node_server(root);
    renders_and_calls("build", &built).await;
}
