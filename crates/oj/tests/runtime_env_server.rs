// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The runtime env of a spawned `oj dev`, over its `/@oj/env`: what only a
//! real process shows (a stop, a restart).

#[allow(dead_code)]
mod start_app;

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
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

    /// `oj dev` with the keys and, when given, a ready file.
    fn serve(&self, ready: Option<&Path>) -> Server {
        let ready = ready.map(|r| r.to_string_lossy().into_owned());
        let mut env = vec![("OJ_RUNTIME_ENV_KEYS", KEYS)];
        if let Some(ready) = &ready {
            env.push(("OJ_RUNTIME_ENV_READY_FILE", ready.as_str()));
        }
        Server::oj_dev_with(&self.root, &env)
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

// The ready file says every key has a value in a running server: absent at
// boot, written once every key is set, removed when one is unset, and
// removed by a server told to stop. Left behind, it said ready with nothing
// serving.
#[test]
fn the_ready_file_follows_the_binding_and_goes_when_the_server_is_stopped() {
    let app = App::new("ready-stop");
    let ready = app.state.join("ready");
    // A previous run's: this server has nothing bound.
    std::fs::write(&ready, "1\n").unwrap();
    let server = app.serve(Some(&ready));
    let (_, status) = env(&server, None).expect("the server answers");
    assert_eq!(status["state"], "unbound", "{status}");
    assert!(!ready.exists(), "unbound at boot");

    set(&server, r#"{"VITE_BACKEND_URL":"/_port_8001"}"#);
    assert!(!ready.exists(), "partial");
    set(&server, BOUND);
    assert!(ready.exists(), "bound: ready");
    set(&server, r#"{"VITE_BACKEND_KEY":null}"#);
    assert!(!ready.exists(), "a key unset: not ready");
    set(&server, BOUND);
    assert!(ready.exists(), "bound again");

    drop(server);
    let deadline = Instant::now() + Duration::from_secs(10);
    while ready.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!ready.exists(), "the stopped server left its ready file");
}

// A value set over `/@oj/env` lives in the server alone. An `.env` edit
// restarts the server: the value is handed to the next image, where it is
// still set, and a key that was not set still is not.
#[test]
fn a_set_value_outlives_a_restart_of_the_server() {
    let app = App::new("restart");
    let server = app.serve(None);
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

// Every key set, with a ready file: the next image is handed every value,
// so it is bound from its first answer and its ready file is there again.
#[test]
fn a_bound_server_is_bound_and_ready_again_after_a_restart() {
    let app = App::new("restart-ready");
    let ready = app.state.join("ready");
    let server = app.serve(Some(&ready));
    set(&server, BOUND);
    assert!(ready.exists(), "bound: ready");
    let status = status_after_restart(&app, &server, "1");
    assert_eq!(status["state"], "bound", "{status}");
    assert_eq!(status["keys"][0]["value"], "/_port_8001", "{status}");
    assert_eq!(status["keys"][1]["value"], "sb_publishable_x", "{status}");
    assert!(ready.exists(), "the next image is bound and serving");
}

// What a restart hands over is what is set when it happens: a value unset
// before it is not set again by it, restart after restart.
#[test]
fn an_unset_value_is_not_brought_back_by_a_restart() {
    let app = App::new("restart-unset");
    let server = app.serve(None);
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
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(
        stderr.contains("OJ_RUNTIME_ENV_KEYS: `VITE_BACKEND_URL=url`"),
        "{stderr}"
    );
}
