// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The runtime env of a spawned `oj dev`, over its `/@oj/env`: what only a
//! real process shows (a restart, a refusal to start).

#[allow(dead_code)]
mod start_app;

use std::io::{Read as _, Write as _};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use start_app::Server;

const KEYS: &str = "VITE_BACKEND_URL,VITE_BACKEND_KEY";
/// Every key, set.
const BOUND: &str = r#"{"VITE_BACKEND_URL":"/_port_8001","VITE_BACKEND_KEY":"sb_publishable_x"}"#;

/// A plain app reading both keys, with the literal of one in `.env`, and a
/// directory beside it for what is not the app's; removed on drop.
struct App {
    root: PathBuf,
    state: PathBuf,
}

impl App {
    fn new(name: &str) -> App {
        let dir =
            std::env::temp_dir().join(format!("oj-runtime-env-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("app/src")).unwrap();
        std::fs::create_dir_all(dir.join("state")).unwrap();
        let dir = dir.canonicalize().unwrap();
        let app = App {
            root: dir.join("app"),
            state: dir.join("state"),
        };
        app.write(
            "index.html",
            "<!doctype html><script type=\"module\" src=\"/src/main.js\"></script>\n",
        );
        app.write(".env", "VITE_BACKEND_URL=https://baked.test\n");
        app.write(
            "src/main.js",
            "export const url = import.meta.env.VITE_BACKEND_URL;\nexport const key = import.meta.env.VITE_BACKEND_KEY;\n",
        );
        app
    }

    fn write(&self, rel: &str, text: &str) {
        std::fs::write(self.root.join(rel), text).unwrap();
    }

    /// `oj dev` with the keys.
    fn serve(&self) -> Server {
        Server::oj_dev_with(&self.root, &[("OJ_RUNTIME_ENV_KEYS", KEYS)])
    }
}

impl Drop for App {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.root.parent().unwrap());
    }
}

/// `POST /@oj/env` with `body`, or `GET` without one: the status code and
/// the JSON answered; `None` while nothing answers (a server restarting).
fn env(server: &Server, body: Option<&str>) -> Option<(u16, serde_json::Value)> {
    let (method, body) = match body {
        Some(body) => ("POST", body),
        None => ("GET", ""),
    };
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", server.port)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    let request = format!(
        "{method} /@oj/env HTTP/1.0\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        server.port,
        body.len()
    );
    stream.write_all(request.as_bytes()).ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    let (head, body) = response.split_once("\r\n\r\n")?;
    let status = head.split(' ').nth(1)?.parse().ok()?;
    Some((status, serde_json::from_str(body).unwrap_or_default()))
}

fn set(server: &Server, body: &str) {
    let (status, answer) = env(server, Some(body)).expect("the server answers");
    assert_eq!(status, 200, "{answer}");
}

/// Edits `.env`, which restarts the server, and returns the status the next
/// image answers with.
fn status_after_restart(app: &App, server: &Server, marker: &str) -> serde_json::Value {
    let mark = server.output().len();
    app.write(
        ".env",
        &format!("VITE_BACKEND_URL=https://baked.test\nVITE_EDITED={marker}\n"),
    );
    let deadline = Instant::now() + Duration::from_secs(40);
    while !server.output()[mark..].contains("restarting dev server") {
        assert!(
            Instant::now() < deadline,
            "the server did not restart:\n{}",
            server.output()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    // The next image answers once it is up; until then there is no server.
    loop {
        if server.output()[mark..].contains("runtime env:") {
            if let Some((200, status)) = env(server, None) {
                return status;
            }
        }
        assert!(
            Instant::now() < deadline,
            "the server did not come back:\n{}",
            server.output()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

// A value set over `/@oj/env` lives in the server alone. An `.env` edit
// restarts the server: the value is handed to the next image, where it is
// still set, and a key that was not set still is not.
#[test]
fn a_set_value_outlives_a_restart_of_the_server() {
    let app = App::new("restart");
    let server = app.serve();
    set(&server, r#"{"VITE_BACKEND_URL":"/_port_9000"}"#);
    let status = status_after_restart(&app, &server, "1");
    assert_eq!(status["state"], "partial", "{status}");
    assert_eq!(status["keys"][0]["value"], "/_port_9000", "{status}");
    assert_eq!(status["keys"][0]["source"], "set", "{status}");
    assert_eq!(
        status["keys"][1]["source"],
        serde_json::Value::Null,
        "{status}"
    );
}

// Every key set: the next image is handed every value, so it is bound from
// its first answer.
#[test]
fn a_bound_server_is_bound_again_after_a_restart() {
    let app = App::new("restart-bound");
    let server = app.serve();
    set(&server, BOUND);
    let status = status_after_restart(&app, &server, "1");
    assert_eq!(status["state"], "bound", "{status}");
    assert_eq!(status["keys"][0]["value"], "/_port_8001", "{status}");
    assert_eq!(status["keys"][1]["value"], "sb_publishable_x", "{status}");
}

// What a restart hands over is what is set when it happens: a value unset
// before it is not set again by it, restart after restart.
#[test]
fn an_unset_value_is_not_brought_back_by_a_restart() {
    let app = App::new("restart-unset");
    let server = app.serve();
    set(
        &server,
        r#"{"VITE_BACKEND_URL":"/_port_9000","VITE_BACKEND_KEY":"k"}"#,
    );
    set(&server, r#"{"VITE_BACKEND_URL":null}"#);

    let status = status_after_restart(&app, &server, "1");
    assert_eq!(status["keys"][0]["source"], "fallback", "{status}");
    assert_eq!(status["keys"][0]["value"], "https://baked.test", "{status}");
    assert_eq!(status["keys"][1]["source"], "set", "{status}");
    assert_eq!(status["keys"][1]["value"], "k", "{status}");

    // Unset in the second image, then restarted again: the first restart's
    // hand-over is not the third image's.
    set(&server, r#"{"VITE_BACKEND_KEY":null}"#);
    let status = status_after_restart(&app, &server, "2");
    assert_eq!(status["state"], "unbound", "{status}");
    assert_eq!(
        status["keys"][1]["source"],
        serde_json::Value::Null,
        "{status}"
    );
}

// `OJ_RUNTIME_ENV_FILE` is no setting: like any variable oj does not read,
// it is no error and nothing is read from the file it names. The keys serve
// their literals and the page is served.
#[test]
fn the_runtime_env_file_variable_is_ignored() {
    let app = App::new("stray-file");
    let stray = app.state.join("env.json");
    std::fs::write(&stray, BOUND).unwrap();
    let server = Server::oj_dev_with(
        &app.root,
        &[
            ("OJ_RUNTIME_ENV_KEYS", KEYS),
            ("OJ_RUNTIME_ENV_FILE", stray.to_str().unwrap()),
        ],
    );
    let (code, status) = env(&server, None).expect("the server answers");
    assert_eq!(code, 200, "{status}");
    assert_eq!(status["state"], "unbound", "{status}");
    assert_eq!(status["generation"], 0, "{status}");
    assert!(status.get("file").is_none(), "{status}");
    assert_eq!(status["keys"][0]["source"], "fallback", "{status}");
    assert_eq!(
        status["keys"][1]["source"],
        serde_json::Value::Null,
        "{status}"
    );
    let (code, page) = server.request("GET", "/", &[]);
    assert_eq!(code, 200, "{page}");
    assert!(page.contains("/src/main.js"), "{page}");
}

// `OJ_RUNTIME_ENV_READY_FILE` is no setting either: no file is written at
// the path it names, with every key set or not, and the status names none.
// `GET /@oj/env` answering `"state": "bound"` is the signal.
#[test]
fn the_runtime_env_ready_file_variable_is_ignored() {
    let app = App::new("stray-ready");
    let ready = app.state.join("ready");
    let server = Server::oj_dev_with(
        &app.root,
        &[
            ("OJ_RUNTIME_ENV_KEYS", KEYS),
            ("OJ_RUNTIME_ENV_READY_FILE", ready.to_str().unwrap()),
        ],
    );
    let (code, status) = env(&server, None).expect("the server answers");
    assert_eq!(code, 200, "{status}");
    assert_eq!(status["state"], "unbound", "{status}");

    set(&server, BOUND);
    let (_, status) = env(&server, None).expect("the server answers");
    assert_eq!(status["state"], "bound", "{status}");
    assert!(status.get("readyFile").is_none(), "{status}");
    assert!(!ready.exists(), "no file is written at the path");
    let mut fields: Vec<&str> = status
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    fields.sort_unstable();
    assert_eq!(
        fields,
        ["generation", "keys", "module", "state"],
        "{status}"
    );
}

// A key mapped to a field (`KEY=field`) is refused at startup, by name: the
// server does not start with a key it would never serve.
#[test]
fn a_mapped_key_is_a_startup_error_naming_the_entry() {
    let app = App::new("mapped-key");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_oj"))
        .arg("dev")
        .arg(&app.root)
        .args(["--port", &start_app::free_port().to_string()])
        .env(
            "OJ_RUNTIME_ENV_KEYS",
            "VITE_BACKEND_KEY,VITE_BACKEND_URL=url",
        )
        .current_dir(&app.root)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // A server that took the entry would run for good: it is given a
    // moment to refuse, then stopped, so the test fails instead of hanging.
    let mut child = out;
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("oj dev started with a mapped key instead of refusing it");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(!status.success(), "{stderr}");
    assert!(
        stderr.contains("OJ_RUNTIME_ENV_KEYS: `VITE_BACKEND_URL=url`"),
        "{stderr}"
    );
}
