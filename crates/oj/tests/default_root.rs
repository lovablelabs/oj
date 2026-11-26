// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! With no root named, `oj dev`, `oj build` and `oj preview` take the
//! directory they start in, as Vite does (`config.root ?
//! path.resolve(config.root) : process.cwd()`). An app that holds a
//! `playground/index.html` of its own is still served and built from its
//! root: oj once took that folder as the root when it was there.

#[allow(dead_code)]
mod start_app;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use start_app::Server;

const APP: &str = "the-app-at-the-root";
const PLAYGROUND: &str = "the-demo-in-playground";

fn page(marker: &str) -> String {
    format!(
        "<!doctype html><html><head><title>{marker}</title></head>\
         <body><script type=\"module\" src=\"/main.js\"></script></body></html>"
    )
}

/// An app whose root and whose `playground/` folder each hold a page.
fn app() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("playground")).unwrap();
    for (file, body) in [
        (
            "package.json",
            r#"{"name":"app","type":"module"}"#.to_string(),
        ),
        ("index.html", page(APP)),
        (
            "main.js",
            format!("document.body.dataset.app = \"{APP}\";\n"),
        ),
        ("playground/index.html", page(PLAYGROUND)),
        (
            "playground/main.js",
            format!("document.body.dataset.app = \"{PLAYGROUND}\";\n"),
        ),
    ] {
        std::fs::write(root.join(file), body).unwrap();
    }
    (tmp, root)
}

fn build_in(cwd: &Path) {
    let out = Command::new(env!("CARGO_BIN_EXE_oj"))
        .arg("build")
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "oj build failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn get(port: u16, path: &str) -> Option<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: localhost:{port}\r\nAccept: text/html\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut body = Vec::new();
    stream.read_to_end(&mut body).ok()?;
    Some(String::from_utf8_lossy(&body).into_owned())
}

/// `oj preview`, stopped on drop: a failed check must not leave it running.
struct Preview(Child);

impl Drop for Preview {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn dev_serves_the_directory_it_starts_in() {
    let (_tmp, root) = app();
    let server = Server::oj_dev_in(&root, &[]);
    let (status, body) = server.request("GET", "/", &[("accept", "text/html".to_string())]);
    assert_eq!(status, 200, "{body}\n{}", server.output());
    assert!(body.contains(APP), "{body}");
    assert!(!body.contains(PLAYGROUND), "{body}");
}

#[test]
fn build_builds_the_directory_it_starts_in() {
    let (_tmp, root) = app();
    build_in(&root);
    let html = std::fs::read_to_string(root.join("dist/index.html")).unwrap();
    assert!(html.contains(APP), "{html}");
    assert!(
        !root.join("playground/dist").exists(),
        "the build went into playground/"
    );
}

#[test]
fn preview_serves_the_build_of_the_directory_it_starts_in() {
    let (tmp, root) = app();
    build_in(&root);
    // A build of the folder too: before the change, preview served this one.
    let out = Command::new(env!("CARGO_BIN_EXE_oj"))
        .args(["build", "playground"])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let port = start_app::free_port();
    // The output goes to a file, read back when a check fails.
    let log = tmp.path().join("preview.log");
    let out = std::fs::File::create(&log).unwrap();
    let mut preview = Preview(
        Command::new(env!("CARGO_BIN_EXE_oj"))
            .args(["preview", "--port", &port.to_string(), "--strictPort"])
            .current_dir(&root)
            .stdout(out.try_clone().unwrap())
            .stderr(out)
            .spawn()
            .unwrap(),
    );
    let output = || std::fs::read_to_string(&log).unwrap_or_default();
    let deadline = Instant::now() + Duration::from_secs(60);
    let body = loop {
        if let Some(body) = get(port, "/").filter(|b| b.contains("<title>")) {
            break body;
        }
        if let Ok(Some(status)) = preview.0.try_wait() {
            panic!("oj preview exited: {status}\n{}", output());
        }
        assert!(
            Instant::now() < deadline,
            "oj preview never answered\n{}",
            output()
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(body.contains(APP), "{body}\n{}", output());
    assert!(!body.contains(PLAYGROUND), "{body}\n{}", output());
}
