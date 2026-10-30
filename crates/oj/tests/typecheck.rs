// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `oj dev --typecheck`: the dev server's `/@oj/typecheck`, and `oj typecheck`.

#[allow(dead_code)]
mod start_app;

use std::path::{Path, PathBuf};
use std::process::Command;

use start_app::Server;

/// A plain app (no dependencies to install) whose `src/main.ts` has one type
/// error, removed on drop.
struct App(PathBuf);

impl App {
    fn new(name: &str) -> App {
        let dir = std::env::temp_dir().join(format!("oj-tc-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let app = App(dir.canonicalize().unwrap());
        app.write(
            "index.html",
            "<!doctype html><script type=\"module\" src=\"/src/main.ts\"></script>\n",
        );
        app.write(
            "tsconfig.json",
            r#"{ "compilerOptions": { "strict": true, "noEmit": true, "lib": ["ES2022", "DOM"] }, "include": ["src"] }"#,
        );
        app.write("src/main.ts", "export const n: number = 'one';\n");
        app
    }

    fn write(&self, rel: &str, text: &str) {
        let path = self.0.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
}

impl Drop for App {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const MAIN_ERROR: &str =
    "src/main.ts(1,14): error TS2322: Type 'string' is not assignable to type 'number'.\n";

/// `oj typecheck <root> [args]`: exit code and stdout.
fn oj_typecheck(root: &Path, args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_oj"))
        .arg("typecheck")
        .args(args)
        .arg(root)
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

/// `oj typecheck` with extra environment variables.
fn oj_typecheck_with(root: &Path, args: &[&str], env: &[(&str, &str)]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_oj"))
        .arg("typecheck")
        .args(args)
        .arg(root)
        .envs(env.iter().copied())
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

/// One `POST /@oj/typecheck` with `body`: the status and the response body.
fn post(server: &Server, body: &str) -> (u16, String) {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", server.port)).unwrap();
    write!(
        stream,
        "POST /@oj/typecheck HTTP/1.0\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
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

/// Polls `/@oj/typecheck` until `done` holds for its report.
fn wait_for_report(
    server: &Server,
    what: &str,
    done: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let (_, body) = server.request("GET", "/@oj/typecheck", &[]);
        let report = json(&body);
        if done(&report) {
            return report;
        }
        assert!(std::time::Instant::now() < deadline, "{what}: {body}");
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

fn json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("{e}: {body}"))
}

#[test]
fn the_dev_server_reports_type_errors_and_an_edit_named_in_the_request() {
    let app = App::new("serve");
    let server = Server::oj_dev_with_args(&app.0, &["--typecheck"], &[]);
    let (status, body) = server.request("GET", "/@oj/typecheck", &[]);
    assert_eq!(status, 200, "{body}");
    let report = json(&body);
    assert_eq!(report["status"], "errors", "{body}");
    assert_eq!(report["text"], MAIN_ERROR);

    // `oj typecheck` asks this server (its generation moves on), and the
    // file it names is re-read even when the watcher has not reported it.
    app.write("src/main.ts", "export const n: number = 1;\n");
    let (code, out) = oj_typecheck(&app.0, &["--changed", "src/main.ts", "--json"]);
    let report = json(&out);
    assert_eq!((code, report["status"].as_str()), (0, Some("ok")), "{out}");
    assert!(report["generation"].as_u64() > Some(1), "{out}");
}

#[test]
fn the_dev_server_picks_up_an_edit_from_the_watcher_alone() {
    let app = App::new("watch");
    let server = Server::oj_dev_with_args(&app.0, &["--typecheck"], &[]);
    let (_, body) = server.request("GET", "/@oj/typecheck", &[]);
    assert_eq!(json(&body)["status"], "errors", "{body}");
    app.write("src/main.ts", "export const n: number = 1;\n");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let (_, body) = server.request("GET", "/@oj/typecheck", &[]);
        if json(&body)["status"] == "ok" {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the edit never reached the checker: {body}"
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

#[test]
fn the_typecheck_route_refuses_browser_and_proxied_requests() {
    let app = App::new("guard");
    let server = Server::oj_dev_with_args(&app.0, &["--typecheck"], &[]);
    let origin = [("Origin", server.url(""))];
    assert_eq!(server.request("GET", "/@oj/typecheck", &origin).0, 403);
    let proxied = [("X-Forwarded-Host", "preview.example".to_string())];
    assert_eq!(server.request("GET", "/@oj/typecheck", &proxied).0, 403);
}

#[test]
fn the_typecheck_route_is_absent_without_the_flag_even_with_the_old_env() {
    let app = App::new("off");
    let server = Server::oj_dev_with(&app.0, &[("OJ_TYPECHECK", "1")]);
    assert_eq!(server.request("GET", "/@oj/typecheck", &[]).0, 404);
}

#[test]
fn oj_typecheck_checks_in_process_when_no_server_answers() {
    let app = App::new("cli");
    assert_eq!(oj_typecheck(&app.0, &[]), (2, MAIN_ERROR.to_string()));
    // An address file left by a server that is gone is ignored.
    let cache = app.0.join(".oj-cache/v1");
    std::fs::create_dir_all(&cache).unwrap();
    let stale = serde_json::json!({
        "root": app.0, "pid": 999999, "host": "127.0.0.1", "port": 1, "https": false,
    });
    std::fs::write(cache.join("dev-server.json"), stale.to_string()).unwrap();
    assert_eq!(oj_typecheck(&app.0, &[]), (2, MAIN_ERROR.to_string()));
    app.write("src/main.ts", "export const n: number = 1;\n");
    assert_eq!(oj_typecheck(&app.0, &[]), (0, String::new()));
}

#[test]
fn oj_typecheck_fails_without_a_tsconfig() {
    let app = App::new("noconfig");
    std::fs::remove_file(app.0.join("tsconfig.json")).unwrap();
    assert_eq!(oj_typecheck(&app.0, &[]).0, 1);
}

#[test]
fn a_post_body_names_the_changed_files_and_bad_json_is_refused() {
    let app = App::new("post");
    let server = Server::oj_dev_with_args(&app.0, &["--typecheck"], &[]);
    app.write("src/main.ts", "export const n: number = 1;\n");
    let body = serde_json::json!({ "changed": [app.0.join("src/main.ts")] }).to_string();
    let (status, report) = post(&server, &body);
    assert_eq!(
        (status, json(&report)["status"].as_str()),
        (200, Some("ok")),
        "{report}"
    );
    assert_eq!(post(&server, "{not json").0, 400);
}

#[test]
fn concurrent_requests_all_get_the_report() {
    let app = App::new("concurrent");
    let server = std::sync::Arc::new(Server::oj_dev_with_args(&app.0, &["--typecheck"], &[]));
    let answers: Vec<_> = (0..8)
        .map(|_| {
            let server = std::sync::Arc::clone(&server);
            std::thread::spawn(move || server.request("GET", "/@oj/typecheck", &[]))
        })
        .collect();
    for answer in answers {
        let (status, body) = answer.join().unwrap();
        assert_eq!(
            (status, json(&body)["text"].as_str()),
            (200, Some(MAIN_ERROR)),
            "{body}"
        );
    }
}

#[test]
fn a_killed_checker_worker_is_replaced_and_the_dev_server_keeps_serving() {
    let app = App::new("crash");
    let server = Server::oj_dev_with_args(&app.0, &["--typecheck"], &[]);
    let (_, body) = server.request("GET", "/@oj/typecheck", &[]);
    assert_eq!(json(&body)["status"], "errors", "{body}");
    let pattern = format!("typecheck-worker {}", app.0.display());
    let killed = Command::new("pkill")
        .args(["-9", "-f", &pattern])
        .status()
        .unwrap();
    assert!(killed.success(), "no worker process for {pattern}");
    let (status, body) = server.request("GET", "/@oj/typecheck", &[]);
    assert_eq!(
        (status, json(&body)["text"].as_str()),
        (200, Some(MAIN_ERROR)),
        "{body}"
    );
    assert_eq!(server.request("GET", "/index.html", &[]).0, 200);
}

#[test]
fn oj_typecheck_ignores_a_server_another_app_advertised_in_a_shared_cache_dir() {
    let busy = App::new("shared-a");
    let clean = App::new("shared-b");
    clean.write("src/main.ts", "export const n: number = 1;\n");
    let cache = std::env::temp_dir().join(format!("oj-tc-cache-{}", std::process::id()));
    let cache_dir = cache.to_string_lossy().into_owned();
    let env = [("OJ_CACHE_DIR", cache_dir.as_str())];
    let _server = Server::oj_dev_with_args(&busy.0, &["--typecheck"], &env);
    assert_eq!(
        oj_typecheck_with(&busy.0, &[], &env),
        (2, MAIN_ERROR.to_string())
    );
    assert_eq!(oj_typecheck_with(&clean.0, &[], &env), (0, String::new()));
    let _ = std::fs::remove_dir_all(&cache);
}

#[test]
fn a_server_for_an_app_without_tsconfig_answers_unavailable() {
    let app = App::new("server-noconfig");
    std::fs::remove_file(app.0.join("tsconfig.json")).unwrap();
    let server = Server::oj_dev_with_args(&app.0, &["--typecheck"], &[]);
    let (status, body) = server.request("GET", "/@oj/typecheck", &[]);
    assert_eq!(
        (status, json(&body)["status"].as_str()),
        (200, Some("unavailable")),
        "{body}"
    );
}

#[test]
fn a_tsconfig_edit_reaches_the_server_through_the_watcher() {
    let app = App::new("watch-config");
    app.write("src/main.ts", "export function f(x) { return x; }\n");
    let server = Server::oj_dev_with_args(&app.0, &["--typecheck"], &[]);
    wait_for_report(&server, "the implicit any", |r| r["status"] == "errors");
    app.write(
        "tsconfig.json",
        r#"{ "compilerOptions": { "strict": false, "noEmit": true, "lib": ["ES2022", "DOM"] }, "include": ["src"] }"#,
    );
    wait_for_report(&server, "the config edit never reached the checker", |r| {
        r["status"] == "ok"
    });
}

#[test]
fn a_directory_moved_away_takes_its_errors_with_it() {
    let app = App::new("watch-move");
    app.write("src/main.ts", "export const n = 1;\n");
    app.write("src/comp/bad.ts", "export const b: number = 'b';\n");
    let server = Server::oj_dev_with_args(&app.0, &["--typecheck"], &[]);
    wait_for_report(&server, "the error in src/comp", |r| r["errorCount"] == 1);
    let outside = std::env::temp_dir().join(format!("oj-tc-moved-{}", std::process::id()));
    std::fs::rename(app.0.join("src/comp"), &outside).unwrap();
    wait_for_report(&server, "the moved directory's error stayed", |r| {
        r["status"] == "ok"
    });
    let _ = std::fs::remove_dir_all(&outside);
}

#[test]
fn a_start_app_dev_server_answers_typecheck() {
    let Some(app) = start_app::app("typecheck") else {
        return;
    };
    let server = Server::oj_dev_with_args(&app.0, &["--typecheck"], &[]);
    let (status, body) = server.request("GET", "/@oj/typecheck", &[]);
    let report = json(&body);
    assert_eq!(status, 200, "{body}");
    assert!(
        matches!(report["status"].as_str(), Some("ok" | "errors")),
        "{body}"
    );
    assert!(report["generation"].as_u64() >= Some(1), "{body}");
}
