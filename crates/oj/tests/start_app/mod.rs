// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The Start fixture app and its servers, shared by the Start test files:
//! each test runs a private copy of e2e/fixtures/start-app, which shares the
//! fixture's installed `node_modules`. The fixture is installed by
//! `npm ci --prefix e2e/fixtures/start-app`; without it the tests skip, unless
//! `OJ_REQUIRE_FIXTURES` is set.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const WAIT: Duration = Duration::from_secs(120);

/// A private copy of the fixture's sources, removed on drop (a failing
/// assert included).
pub struct App(pub PathBuf);

impl Drop for App {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The app, or None (after saying why) when the fixture is not installed.
pub fn app(name: &str) -> Option<App> {
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
        .join(format!("oj-start-app-{name}-{}", std::process::id()));
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

pub fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A server process, stopped on drop, whose stdout and stderr are kept
/// together: either may carry an SSR error.
pub struct Server {
    child: Child,
    pub port: u16,
    output: Arc<Mutex<String>>,
}

impl Server {
    /// `oj dev <root>`, on the port it reports: oj moves to the next port
    /// when the one asked for was taken in the meantime.
    pub fn oj_dev(root: &Path) -> Server {
        Server::oj_dev_with(root, &[])
    }

    /// `oj dev <root>` with extra environment variables.
    pub fn oj_dev_with(root: &Path, env: &[(&str, &str)]) -> Server {
        let mut server = Server::spawn(
            Command::new(env!("CARGO_BIN_EXE_oj"))
                .arg("dev")
                .arg(root)
                .args(["--port", &free_port().to_string()])
                .envs(env.iter().copied())
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
    pub fn node_server(root: &Path) -> Server {
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

    pub fn output(&self) -> String {
        self.output.lock().unwrap().clone()
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    /// Whether the output shows `needle` within a few seconds (a server logs
    /// an error after it answered).
    pub fn logs(&self, needle: &str) -> bool {
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
    pub fn request(&self, method: &str, path: &str, headers: &[(&str, String)]) -> (u16, String) {
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
    pub fn call(&self, id: &str) -> (u16, String) {
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
pub fn listening_port(output: &str) -> Option<u16> {
    let rest = &output[output.find("http://localhost:")? + "http://localhost:".len()..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// `oj build <root>` on its own: no cache an earlier dev run left behind.
pub fn build(root: &Path) {
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
}
