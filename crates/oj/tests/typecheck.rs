// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `OJ_TYPECHECK`: the dev server's `/@oj/typecheck` and `oj typecheck`.

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
        std::fs::write(self.0.join(rel), text).unwrap();
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

fn json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("{e}: {body}"))
}

#[test]
fn the_dev_server_reports_type_errors_and_an_edit_named_in_the_request() {
    let app = App::new("serve");
    let server = Server::oj_dev_with(&app.0, &[("OJ_TYPECHECK", "1")]);
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
    let server = Server::oj_dev_with(&app.0, &[("OJ_TYPECHECK", "1")]);
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
    let server = Server::oj_dev_with(&app.0, &[("OJ_TYPECHECK", "1")]);
    let origin = [("Origin", server.url(""))];
    assert_eq!(server.request("GET", "/@oj/typecheck", &origin).0, 403);
    let proxied = [("X-Forwarded-Host", "preview.example".to_string())];
    assert_eq!(server.request("GET", "/@oj/typecheck", &proxied).0, 403);
}

#[test]
fn the_typecheck_route_is_absent_without_oj_typecheck() {
    let app = App::new("off");
    let server = Server::oj_dev(&app.0);
    assert_eq!(server.request("GET", "/@oj/typecheck", &[]).0, 404);
}

#[test]
fn oj_typecheck_checks_in_process_when_no_server_answers() {
    let app = App::new("cli");
    assert_eq!(oj_typecheck(&app.0, &[]), (2, MAIN_ERROR.to_string()));
    // An address file left by a server that is gone is ignored.
    let cache = app.0.join(".oj-cache/v1");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(
        cache.join("dev-server.json"),
        r#"{"pid":999999,"host":"127.0.0.1","port":1,"https":false}"#,
    )
    .unwrap();
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
