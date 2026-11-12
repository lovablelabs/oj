// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `oj env get|set|unset`: the runtime env of the app's running dev server,
//! over the address it advertised.

#[allow(dead_code)]
mod start_app;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use start_app::Server;

const KEYS: &str = "VITE_BACKEND_URL=url,VITE_BACKEND_KEY=anonKey";
const BOUND: &str = r#"{"url":"/_port_8001","anonKey":"sb_publishable_x"}"#;
const PENDING: &str = r#"{"pending":true}"#;

/// A plain app reading both keys, with the literal of one in `.env`, and a
/// values file beside it; removed on drop.
struct App {
    root: PathBuf,
    values: PathBuf,
}

impl App {
    fn new(name: &str) -> App {
        let dir = std::env::temp_dir().join(format!("oj-env-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("app/src")).unwrap();
        std::fs::create_dir_all(dir.join("values")).unwrap();
        let dir = dir.canonicalize().unwrap();
        let app = App {
            root: dir.join("app"),
            values: dir.join("values/env.json"),
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

    fn publish(&self, text: &str) {
        // A rename, so the watcher never reads a half-written file.
        let tmp = self.values.with_extension("tmp");
        std::fs::write(&tmp, text).unwrap();
        std::fs::rename(&tmp, &self.values).unwrap();
    }

    /// `oj dev` with the keys and the values file.
    fn serve(&self) -> Server {
        let values = self.values.to_string_lossy().into_owned();
        Server::oj_dev_with(
            &self.root,
            &[
                ("OJ_RUNTIME_ENV_KEYS", KEYS),
                ("OJ_RUNTIME_ENV_FILE", values.as_str()),
            ],
        )
    }
}

impl Drop for App {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.root.parent().unwrap());
    }
}

/// `oj env <args>` run from `cwd`: exit code, stdout and stderr.
fn oj_env_in(cwd: &Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_oj"))
        .arg("env")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// `oj env <args> --root <root>`.
fn oj_env(app: &App, args: &[&str]) -> (i32, String, String) {
    let root = app.root.to_string_lossy().into_owned();
    let mut all: Vec<&str> = args.to_vec();
    all.extend(["--root", root.as_str()]);
    oj_env_in(&std::env::temp_dir(), &all)
}

fn json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("{e}: {body}"))
}

/// The server's own `GET /@oj/env`, to check what the command reported.
fn server_status(server: &Server) -> serde_json::Value {
    let (status, body) = server.request("GET", "/@oj/env", &[]);
    assert_eq!(status, 200, "{body}");
    json(&body)
}

#[test]
fn get_prints_the_values_in_force_as_dotenv_lines_or_json() {
    let app = App::new("get");
    app.publish(BOUND);
    let _server = app.serve();
    assert_eq!(
        oj_env(&app, &["get"]),
        (
            0,
            "VITE_BACKEND_URL=/_port_8001\nVITE_BACKEND_KEY=sb_publishable_x\n".to_string(),
            String::new()
        )
    );
    // Named keys narrow the lines, in the server's order.
    let (code, out, _) = oj_env(&app, &["get", "VITE_BACKEND_KEY"]);
    assert_eq!(
        (code, out.as_str()),
        (0, "VITE_BACKEND_KEY=sb_publishable_x\n")
    );
    let (code, out, _) = oj_env(&app, &["get", "--json"]);
    let status = json(&out);
    assert_eq!(code, 0, "{out}");
    assert_eq!(status["state"], "bound");
    assert_eq!(status["keys"][0]["source"], "file");
    assert_eq!(status["file"]["state"], "complete");
    let (code, out, _) = oj_env(&app, &["get", "--json", "VITE_BACKEND_URL"]);
    let status = json(&out);
    assert_eq!(code, 0);
    assert_eq!(status["keys"].as_array().unwrap().len(), 1);
    assert_eq!(status["keys"][0]["key"], "VITE_BACKEND_URL");
}

#[test]
fn get_comments_out_a_key_without_a_value() {
    let app = App::new("unset-key");
    let _server = app.serve();
    // No file yet: the URL has its `.env` literal, the key has nothing.
    assert_eq!(
        oj_env(&app, &["get"]),
        (
            0,
            "VITE_BACKEND_URL=https://baked.test\n# VITE_BACKEND_KEY unset\n".to_string(),
            String::new()
        )
    );
}

#[test]
fn set_wins_over_the_file_until_unset_and_the_server_reloads_the_pages() {
    let app = App::new("set");
    app.publish(BOUND);
    let server = app.serve();
    let (code, out, err) = oj_env(
        &app,
        &[
            "set",
            "VITE_BACKEND_URL=/_port_9000",
            "VITE_BACKEND_KEY",
            "sb=with=equals",
        ],
    );
    assert_eq!(
        (code, out.as_str(), err.as_str()),
        (
            0,
            "VITE_BACKEND_URL=/_port_9000\nVITE_BACKEND_KEY=sb=with=equals\n",
            ""
        )
    );
    let status = server_status(&server);
    assert_eq!(status["keys"][0]["source"], "set", "{status}");
    assert_eq!(status["keys"][1]["value"], "sb=with=equals");
    assert_eq!(status["generation"], 2, "{status}");
    assert!(server.logs("runtime env set -> bound (generation 2) -> full-reload"));
    // The module the page loads carries the set values.
    let (_, module) = server.request("GET", "/@oj/env.js", &[]);
    assert!(module.contains("\"/_port_9000\""), "{module}");

    let (code, out, _) = oj_env(&app, &["unset", "VITE_BACKEND_URL"]);
    assert_eq!(
        (code, out.as_str()),
        (
            0,
            "VITE_BACKEND_URL=/_port_8001\nVITE_BACKEND_KEY=sb=with=equals\n"
        )
    );
    let status = server_status(&server);
    assert_eq!(status["keys"][0]["source"], "file");
    assert_eq!(status["keys"][1]["source"], "set");

    // Setting the value already in force changes nothing: no new generation.
    let (code, _, _) = oj_env(&app, &["set", "VITE_BACKEND_KEY=sb=with=equals"]);
    assert_eq!(code, 0);
    assert_eq!(server_status(&server)["generation"], 3);
}

#[test]
fn set_can_bind_a_server_that_has_no_values_file() {
    let app = App::new("no-file");
    let _server = Server::oj_dev_with(&app.root, &[("OJ_RUNTIME_ENV_KEYS", KEYS)]);
    // The URL's `.env` literal is in force but binds nothing.
    let (code, out, _) = oj_env(&app, &["get", "--json"]);
    assert_eq!(code, 0);
    let status = json(&out);
    assert_eq!(status["state"], "unbound", "{out}");
    assert_eq!(status["keys"][0]["value"], "https://baked.test");
    assert_eq!(status["keys"][0]["source"], "fallback");
    let (code, out, _) = oj_env(&app, &["set", "VITE_BACKEND_KEY=k", "--json"]);
    assert_eq!(code, 0);
    assert_eq!(json(&out)["state"], "partial", "{out}");
    let (code, out, _) = oj_env(&app, &["set", "VITE_BACKEND_URL=/_port_1", "--json"]);
    assert_eq!(code, 0);
    assert_eq!(json(&out)["state"], "bound", "{out}");
}

#[test]
fn wait_returns_once_the_file_binds() {
    let app = App::new("wait");
    app.publish(PENDING);
    let _server = app.serve();
    let (code, out, _) = oj_env(&app, &["get", "--json"]);
    assert_eq!(code, 0);
    assert_eq!(json(&out)["state"], "pending", "{out}");

    let started = Instant::now();
    let waiting = {
        let root = app.root.clone();
        std::thread::spawn(move || {
            let root = root.to_string_lossy().into_owned();
            oj_env_in(
                &std::env::temp_dir(),
                &["get", "--wait", "--timeout", "20", "--root", &root],
            )
        })
    };
    std::thread::sleep(Duration::from_millis(600));
    assert!(!waiting.is_finished(), "returned before the file bound");
    app.publish(BOUND);
    let (code, out, err) = waiting.join().unwrap();
    assert_eq!(
        (code, out.as_str(), err.as_str()),
        (
            0,
            "VITE_BACKEND_URL=/_port_8001\nVITE_BACKEND_KEY=sb_publishable_x\n",
            ""
        )
    );
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn wait_for_named_keys_needs_only_those() {
    let app = App::new("wait-named");
    // The file carries the URL and not yet the key: waiting for the URL
    // alone returns at once, waiting for the key times out.
    app.publish(r#"{"url":"/_port_8001"}"#);
    let _server = app.serve();
    let (code, out, _) = oj_env(
        &app,
        &["get", "--wait", "--timeout", "5", "VITE_BACKEND_URL"],
    );
    assert_eq!((code, out.as_str()), (0, "VITE_BACKEND_URL=/_port_8001\n"));
    let started = Instant::now();
    let (code, out, err) = oj_env(
        &app,
        &["get", "--wait", "--timeout", "0.6", "VITE_BACKEND_KEY"],
    );
    assert_eq!(
        (code, out.as_str()),
        (3, "# VITE_BACKEND_KEY unset\n"),
        "{err}"
    );
    assert!(err.contains("still pending after 0.6s"), "{err}");
    assert!(started.elapsed() >= Duration::from_millis(600));
}

#[test]
fn a_literal_does_not_satisfy_a_wait_for_its_key() {
    let app = App::new("wait-literal");
    let _server = app.serve();
    // No file: the URL reads its `.env` literal, which is not a binding.
    let (code, out, err) = oj_env(
        &app,
        &["get", "--wait", "--timeout", "0.5", "VITE_BACKEND_URL"],
    );
    assert_eq!(
        (code, out.as_str()),
        (3, "VITE_BACKEND_URL=https://baked.test\n"),
        "{err}"
    );
    assert!(err.contains("still unbound after 0.5s"), "{err}");
}

#[test]
fn set_with_wait_waits_for_the_keys_it_did_not_set() {
    let app = App::new("set-wait");
    let _server = app.serve();
    // Setting one key leaves the other unset: the wait times out with the
    // set value in force and printed.
    let (code, out, err) = oj_env(
        &app,
        &[
            "set",
            "VITE_BACKEND_URL=/_port_1",
            "--wait",
            "--timeout",
            "0.5",
        ],
    );
    assert_eq!(code, 3, "{out}{err}");
    assert_eq!(out, "VITE_BACKEND_URL=/_port_1\n# VITE_BACKEND_KEY unset\n");
    // Setting both binds at once: no wait.
    let (code, out, _) = oj_env(
        &app,
        &["set", "VITE_BACKEND_KEY=k", "--wait", "--timeout", "5"],
    );
    assert_eq!(
        (code, out.as_str()),
        (0, "VITE_BACKEND_URL=/_port_1\nVITE_BACKEND_KEY=k\n")
    );
}

#[test]
fn without_a_server_or_the_keys_the_command_says_so_and_exits_1() {
    let app = App::new("none");
    assert_eq!(
        oj_env(&app, &["get"]),
        (
            1,
            String::new(),
            "oj env: no dev server found\n".to_string()
        )
    );
    assert_eq!(
        oj_env(&app, &["set", "A=1"]),
        (
            1,
            String::new(),
            "oj env: no dev server found\n".to_string()
        )
    );
    let _server = Server::oj_dev(&app.root);
    assert_eq!(
        oj_env(&app, &["get"]),
        (
            1,
            String::new(),
            "oj env: the dev server runs without OJ_RUNTIME_ENV_KEYS\n".to_string()
        )
    );
}

#[test]
fn a_key_outside_the_list_is_refused_with_the_list() {
    let app = App::new("unknown");
    app.publish(BOUND);
    let _server = app.serve();
    let (code, out, err) = oj_env(&app, &["set", "VITE_OTHER=1", "VITE_BACKEND_URL=/x"]);
    assert_eq!((code, out.as_str()), (1, ""));
    assert!(
        err.contains("unknown runtime env key(s): VITE_OTHER")
            && err.contains("VITE_BACKEND_URL, VITE_BACKEND_KEY"),
        "{err}"
    );
    // Refused as a whole: the valid key did not change either.
    assert_eq!(
        oj_env(&app, &["get", "VITE_BACKEND_URL"]).1,
        "VITE_BACKEND_URL=/_port_8001\n"
    );
    let (code, out, err) = oj_env(&app, &["unset", "VITE_OTHER"]);
    assert_eq!((code, out.as_str()), (1, ""));
    assert!(
        err.contains("unknown runtime env key(s): VITE_OTHER"),
        "{err}"
    );
    let (code, out, err) = oj_env(&app, &["get", "VITE_OTHER", "VITE_BACKEND_URL"]);
    assert_eq!((code, out.as_str()), (1, ""));
    assert!(
        err.contains(
            "unknown runtime env key VITE_OTHER; the keys are VITE_BACKEND_URL, VITE_BACKEND_KEY"
        ),
        "{err}"
    );
}

#[test]
fn malformed_pairs_are_a_usage_error_before_any_request() {
    let app = App::new("usage");
    // No server runs: a request would say so with exit 1.
    let (code, out, err) = oj_env(&app, &["set", "A=1", "B"]);
    assert_eq!((code, out.as_str()), (2, ""));
    assert!(err.contains("no value for `B`"), "{err}");
    let (code, _, err) = oj_env(&app, &["set", "=1"]);
    assert_eq!(code, 2);
    assert!(err.contains("names no key"), "{err}");
    let (code, _, err) = oj_env(&app, &["set", "A=1", "A=2"]);
    assert_eq!(code, 2);
    assert!(err.contains("given twice"), "{err}");
}

#[test]
fn the_root_defaults_to_the_directory_the_command_runs_in() {
    let app = App::new("cwd");
    app.publish(BOUND);
    let _server = app.serve();
    let (code, out, _) = oj_env_in(&app.root, &["get", "VITE_BACKEND_URL"]);
    assert_eq!((code, out.as_str()), (0, "VITE_BACKEND_URL=/_port_8001\n"));
    // Another directory finds no server.
    let (code, _, err) = oj_env_in(app.root.parent().unwrap(), &["get"]);
    assert_eq!((code, err.as_str()), (1, "oj env: no dev server found\n"));
}
