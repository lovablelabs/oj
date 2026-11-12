// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Runtime env keys (`OJ_RUNTIME_ENV_*`): a module that reads one compiles to
//! a read of the global `/@oj/env.js` defines and imports that module first;
//! the values come from a watched file or a local `POST /@oj/env`, a change
//! reloads the pages, documents wait while the file is incomplete, and the
//! compiled modules never change across a rebind.

use std::path::PathBuf;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use oj_env::RuntimeKey;
use oj_server::RuntimeEnvConfig;

use crate::listen::{close, frame_matching};
use crate::{App, Response, Server};

const INDEX: &str = r#"<!doctype html><html><head><meta name="backend" content="%VITE_BACKEND_URL%"></head><body><script type="module" src="/src/main.js"></script></body></html>"#;
const MAIN: &str = "export const backend = import.meta.env.VITE_BACKEND_URL;\nexport const key = import.meta.env.VITE_BACKEND_KEY;\nexport const mode = import.meta.env.MODE;\n";
const PLAIN: &str = "export const title = import.meta.env.VITE_TITLE;\n";
const BOUND: &str = r#"{"url":"/_port_8001","anonKey":"sb_publishable_x","projectId":""}"#;
const PENDING: &str = r#"{"pending":true}"#;
const WAIT: Duration = Duration::from_secs(15);

fn app() -> App {
    App::new(&[
        ("package.json", r#"{"name":"runtime-env","private":true}"#),
        (
            ".env",
            "VITE_BACKEND_URL=https://baked.test\nVITE_TITLE=Hello\n",
        ),
        ("index.html", INDEX),
        ("src/main.js", MAIN),
        ("src/plain.js", PLAIN),
    ])
}

fn keys() -> Vec<RuntimeKey> {
    vec![
        RuntimeKey {
            key: "VITE_BACKEND_URL".into(),
            field: "url".into(),
        },
        RuntimeKey {
            key: "VITE_BACKEND_KEY".into(),
            field: "anonKey".into(),
        },
    ]
}

/// A directory beside the app, outside its root: where a backend keeps its
/// state, and what the watcher must follow although the root watch does not
/// cover it.
fn values_dir(app: &App) -> PathBuf {
    let dir = app.root.with_file_name(format!(
        "{}-values",
        app.root.file_name().unwrap().to_str().unwrap()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn serve_with(app: &App, file: Option<PathBuf>, ready: Option<PathBuf>) -> Server {
    let mut dev = app.dev();
    dev.runtime_env = Some(RuntimeEnvConfig {
        keys: keys(),
        file,
        ready_file: ready,
    });
    crate::serve(dev).await
}

async fn post(server: &Server, body: &str, headers: &[(&str, &str)]) -> Response {
    let mut req = Request::post("/@oj/env").header("content-type", "application/json");
    for (name, value) in headers {
        req = req.header(*name, *value);
    }
    server
        .request(req.body(Body::from(body.to_string())).unwrap())
        .await
}

async fn status(server: &Server) -> serde_json::Value {
    let res = server.get("/@oj/env").await;
    assert_eq!(res.status, 200, "{}", res.body);
    serde_json::from_str(&res.body).unwrap()
}

/// Applies `write` and polls the status until `done` accepts it, applying
/// `write` again every second meanwhile: a watch added on the fly can open
/// after the first write lands (as css_outside_root_deps retries too), and
/// a backend's writer would write again or be read on the next change.
/// Fails after `WAIT`.
async fn settle(
    server: &Server,
    write: impl Fn(),
    done: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + WAIT;
    let mut last_write = tokio::time::Instant::now();
    write();
    loop {
        let status = status(server).await;
        if done(&status) {
            return status;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the runtime env never settled: {status}"
        );
        if last_write.elapsed() > Duration::from_secs(1) {
            write();
            last_write = tokio::time::Instant::now();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn header<'a>(res: &'a Response, name: &str) -> Option<&'a str> {
    res.headers.get(name).and_then(|v| v.to_str().ok())
}

fn first_statement(code: &str) -> &str {
    code.trim_start().split_inclusive(';').next().unwrap_or("")
}

#[tokio::test(flavor = "multi_thread")]
async fn redirected_modules_import_the_env_module_and_serve_the_literal_until_bound() {
    let app = app();
    let server = serve_with(&app, None, None).await;

    let main = server.text("/src/main.js").await;
    assert_eq!(
        first_statement(&main),
        "import \"/@oj/env.js\";",
        "the env module is imported first:\n{main}"
    );
    assert!(
        main.contains("globalThis.__oj_env.VITE_BACKEND_URL"),
        "{main}"
    );
    assert!(
        main.contains("globalThis.__oj_env.VITE_BACKEND_KEY"),
        "{main}"
    );
    assert!(!main.contains("baked.test"), "no literal baked in:\n{main}");
    assert!(
        main.contains("\"development\""),
        "MODE stays a literal:\n{main}"
    );

    let plain = server.text("/src/plain.js").await;
    assert!(
        !plain.contains("/@oj/env.js"),
        "a module reading no runtime key imports nothing:\n{plain}"
    );
    assert!(plain.contains("\"Hello\""), "{plain}");

    let env = server.get("/@oj/env.js").await;
    assert_eq!(env.status, 200, "{}", env.body);
    assert_eq!(header(&env, "cache-control"), Some("no-store"));
    assert!(header(&env, "content-type").is_some_and(|t| t.starts_with("text/javascript")));
    assert!(
        env.body
            .contains("\"VITE_BACKEND_URL\": \"https://baked.test\""),
        "the literal until bound:\n{}",
        env.body
    );
    assert!(
        env.body.contains("\"VITE_TITLE\": \"Hello\""),
        "{}",
        env.body
    );
    assert!(
        env.body.contains("\"MODE\": \"development\""),
        "{}",
        env.body
    );
    assert!(
        !env.body.contains("VITE_BACKEND_KEY"),
        "a key the env lacks is left out:\n{}",
        env.body
    );

    let html = server.text("/").await;
    assert!(html.contains(r#"content="https://baked.test""#), "{html}");

    let status = status(&server).await;
    assert_eq!(status["state"], "unbound");
    assert_eq!(status["generation"], 0);
    assert_eq!(status["file"]["state"], "absent");
    assert_eq!(status["module"], "/@oj/env.js");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pending_values_file_holds_documents_and_serves_modules() {
    let app = app();
    let file = values_dir(&app).join("env.json");
    std::fs::write(&file, PENDING).unwrap();
    let server = serve_with(&app, Some(file), None).await;

    for path in ["/", "/index.html", "/some/route"] {
        let res = server.get(path).await;
        assert_eq!(res.status, 503, "{path}: {}", res.body);
        assert_eq!(header(&res, "cache-control"), Some("no-store"), "{path}");
        assert_eq!(header(&res, "retry-after"), Some("1"), "{path}");
        assert!(
            res.body.contains("runtime env pending"),
            "{path}: {}",
            res.body
        );
    }
    assert_eq!(
        server.get("/src/main.js").await.status,
        200,
        "modules still serve"
    );
    assert_eq!(server.get("/src/plain.js").await.status, 200);
    assert_eq!(server.get("/@oj/env.js").await.status, 200);
    assert_eq!(server.get("/@oj/client.js").await.status, 200);

    let status = status(&server).await;
    assert_eq!(status["state"], "pending");
    assert_eq!(status["file"]["state"], "pending");
    assert_eq!(
        status["file"]["missing"],
        serde_json::json!(["VITE_BACKEND_URL", "VITE_BACKEND_KEY"])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bound_values_file_reaches_the_module_the_page_and_the_ready_file() {
    let app = app();
    let dir = values_dir(&app);
    let file = dir.join("env.json");
    let ready = dir.join("ready");
    std::fs::write(&file, BOUND).unwrap();
    let server = serve_with(&app, Some(file.clone()), Some(ready.clone())).await;

    let env = server.text("/@oj/env.js").await;
    assert!(
        env.contains("\"VITE_BACKEND_URL\": \"/_port_8001\""),
        "{env}"
    );
    assert!(
        env.contains("\"VITE_BACKEND_KEY\": \"sb_publishable_x\""),
        "{env}"
    );
    assert!(!env.contains("baked.test"), "{env}");

    let html = server.text("/").await;
    assert!(
        html.contains(r#"content="/_port_8001""#),
        "%KEY% follows the value in force:\n{html}"
    );

    let status = status(&server).await;
    assert_eq!(status["state"], "bound");
    assert_eq!(status["generation"], 1);
    assert_eq!(status["file"]["state"], "complete");
    assert_eq!(status["file"]["path"], file.display().to_string());
    assert_eq!(status["keys"][0]["source"], "file");
    assert_eq!(std::fs::read_to_string(&ready).unwrap(), "1\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_local_post_sets_values_and_reloads_the_pages() {
    let app = app();
    let server = serve_with(&app, None, None).await;
    let listening = server.listen().await;
    let (mut socket, _) = server.ws(listening.addr, "/__ws", None).await;

    let res = post(&server, r#"{"VITE_BACKEND_URL":"/_port_9000"}"#, &[]).await;
    assert_eq!(res.status, 200, "{}", res.body);
    assert_eq!(header(&res, "cache-control"), Some("no-store"));
    let status: serde_json::Value = serde_json::from_str(&res.body).unwrap();
    assert_eq!(
        status["state"], "partial",
        "one of two keys set, no file: {status}"
    );
    assert_eq!(status["generation"], 1);
    assert_eq!(status["keys"][0]["value"], "/_port_9000");
    assert_eq!(status["keys"][0]["source"], "set");
    assert_eq!(status["keys"][1]["source"], serde_json::Value::Null);

    let frame = frame_matching(&mut socket, "the reload onto the new values", |f| {
        f["type"] == "full-reload" && f["reason"] == "runtime env"
    })
    .await;
    assert_eq!(frame["path"], "*");

    let env = server.text("/@oj/env.js").await;
    assert!(
        env.contains("\"VITE_BACKEND_URL\": \"/_port_9000\""),
        "{env}"
    );
    let html = server.text("/").await;
    assert!(html.contains(r#"content="/_port_9000""#), "{html}");

    // The same value again changes nothing.
    let res = post(&server, r#"{"VITE_BACKEND_URL":"/_port_9000"}"#, &[]).await;
    assert_eq!(res.status, 200);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&res.body).unwrap()["generation"],
        1
    );

    // Unsetting falls back to the literal.
    let res = post(&server, r#"{"VITE_BACKEND_URL":null}"#, &[]).await;
    assert_eq!(res.status, 200);
    let status: serde_json::Value = serde_json::from_str(&res.body).unwrap();
    assert_eq!(status["state"], "unbound");
    assert_eq!(status["generation"], 2);
    assert_eq!(status["keys"][0]["source"], "fallback");
    frame_matching(&mut socket, "the reload back onto the literal", |f| {
        f["type"] == "full-reload" && f["reason"] == "runtime env"
    })
    .await;
    assert!(server
        .text("/@oj/env.js")
        .await
        .contains("\"https://baked.test\""));
    close(socket).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_control_endpoint_refuses_browsers_proxies_and_bad_bodies() {
    let app = app();
    let server = serve_with(&app, None, None).await;
    let body = r#"{"VITE_BACKEND_URL":"/evil"}"#;

    let res = post(&server, body, &[("origin", "http://localhost:5199")]).await;
    assert_eq!(
        res.status, 403,
        "a page may not repoint the backend: {}",
        res.body
    );
    let res = post(&server, body, &[("x-forwarded-host", "preview.example")]).await;
    assert_eq!(
        res.status, 403,
        "nor may anything through the preview proxy: {}",
        res.body
    );
    let res = server
        .request(
            Request::get("/@oj/env")
                .header("origin", "http://localhost:5199")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(res.status, 403, "the status is local too");

    let res = post(&server, r#"{"VITE_NOPE":"x","VITE_BACKEND_URL":"/a"}"#, &[]).await;
    assert_eq!(res.status, 400);
    assert!(res.body.contains("VITE_NOPE"), "{}", res.body);
    let res = post(&server, r#"["VITE_BACKEND_URL"]"#, &[]).await;
    assert_eq!(res.status, 400);
    let res = post(&server, r#"{"VITE_BACKEND_URL":1}"#, &[]).await;
    assert_eq!(res.status, 400);
    assert!(res.body.contains("a number"), "{}", res.body);
    let res = post(&server, "not json", &[]).await;
    assert_eq!(res.status, 400);

    let res = server
        .request(Request::delete("/@oj/env").body(Body::empty()).unwrap())
        .await;
    assert_eq!(res.status, 405);

    let status = status(&server).await;
    assert_eq!(status["generation"], 0, "nothing refused applied: {status}");
    assert_eq!(status["state"], "unbound");

    // The module itself is what pages load: it answers a browser.
    let res = server
        .request(
            Request::get("/@oj/env.js")
                .header("origin", "http://localhost:5199")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(res.status, 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_write_to_the_values_file_rebinds_without_a_restart() {
    let app = app();
    let dir = values_dir(&app);
    let file = dir.join("env.json");
    let ready = dir.join("ready");
    std::fs::write(&file, PENDING).unwrap();
    let server = serve_with(&app, Some(file.clone()), Some(ready.clone())).await;
    let listening = server.listen().await;
    let (mut socket, _) = server.ws(listening.addr, "/__ws", None).await;
    assert_eq!(server.get("/").await.status, 503);
    assert!(!ready.exists());

    // The backend comes up: its file completes, landed by rename.
    let tmp = dir.join("env.json.tmp");
    let seen = settle(
        &server,
        || {
            std::fs::write(&tmp, BOUND).unwrap();
            std::fs::rename(&tmp, &file).unwrap();
        },
        |s| s["state"] == "bound",
    )
    .await;
    assert_eq!(seen["generation"], 1, "{seen}");
    let frame = frame_matching(&mut socket, "the reload onto the bound values", |f| {
        f["type"] == "full-reload" && f["reason"] == "runtime env"
    })
    .await;
    assert_eq!(frame["triggeredBy"], file.display().to_string());
    assert_eq!(server.get("/").await.status, 200);
    assert!(server.text("/@oj/env.js").await.contains("/_port_8001"));
    assert_eq!(std::fs::read_to_string(&ready).unwrap(), "1\n");

    // It goes back to pending: documents wait again.
    let seen = settle(
        &server,
        || std::fs::write(&file, PENDING).unwrap(),
        |s| s["state"] == "pending",
    )
    .await;
    assert_eq!(seen["generation"], 2, "{seen}");
    frame_matching(&mut socket, "the reload off the values", |f| {
        f["type"] == "full-reload" && f["reason"] == "runtime env"
    })
    .await;
    assert_eq!(server.get("/").await.status, 503);
    assert!(!ready.exists());

    // Removed: the literals, and documents serve.
    settle(
        &server,
        || {
            let _ = std::fs::remove_file(&file);
        },
        |s| s["state"] == "unbound",
    )
    .await;
    assert_eq!(server.get("/").await.status, 200);
    assert!(server.text("/@oj/env.js").await.contains("baked.test"));
    close(socket).await;
    // `Server::drop` fails the test if any of this asked for a restart.
}

#[tokio::test(flavor = "multi_thread")]
async fn compiled_modules_are_stable_across_a_rebind() {
    let app = app();
    let server = serve_with(&app, None, None).await;
    let before = server.text("/src/main.js").await;
    let env_before = server.text("/@oj/env.js").await;
    let res = post(
        &server,
        r#"{"VITE_BACKEND_URL":"/_port_8001","VITE_BACKEND_KEY":"k"}"#,
        &[],
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.body);
    let after = server.text("/src/main.js").await;
    assert_eq!(
        before, after,
        "a rebind changes what one route serves, not the compiled module"
    );
    assert_ne!(env_before, server.text("/@oj/env.js").await);
}

/// The one visible change for an app with no runtime keys: the two routes
/// exist and answer 404 (`/@oj/env` used to fall to the SPA page).
#[tokio::test(flavor = "multi_thread")]
async fn without_runtime_keys_only_the_two_routes_appear() {
    let app = app();
    let server = app.serve().await;
    assert_eq!(server.get("/@oj/env.js").await.status, 404);
    assert_eq!(server.get("/@oj/env").await.status, 404);
    assert_eq!(
        post(&server, r#"{"VITE_BACKEND_URL":"/a"}"#, &[])
            .await
            .status,
        404
    );
    let main = server.text("/src/main.js").await;
    assert!(main.contains("\"https://baked.test\""), "{main}");
    assert!(
        !main.contains("/@oj/env.js") && !main.contains("__oj_env"),
        "{main}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn under_a_base_the_env_module_is_imported_and_served_under_it() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"runtime-env-base","private":true}"#,
        ),
        ("oj.config.json", r#"{"base":"/app/"}"#),
        (".env", "VITE_BACKEND_URL=https://baked.test\n"),
        ("index.html", INDEX),
        ("src/main.js", MAIN),
    ]);
    let server = serve_with(&app, None, None).await;
    let main = server.text("/app/src/main.js").await;
    assert_eq!(
        first_statement(&main),
        "import \"/app/@oj/env.js\";",
        "{main}"
    );
    let env = server.text("/app/@oj/env.js").await;
    assert!(
        env.contains("\"VITE_BACKEND_URL\": \"https://baked.test\""),
        "{env}"
    );
    assert_eq!(
        status(&server).await["module"],
        "/app/@oj/env.js",
        "the unbased status route answers too, as unbased module urls do"
    );
    let res = server.get("/app/@oj/env").await;
    assert_eq!(res.status, 200, "{}", res.body);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&res.body).unwrap()["module"],
        "/app/@oj/env.js"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_inline_page_module_reads_through_the_global() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"runtime-env-inline","private":true}"#,
        ),
        (".env", "VITE_BACKEND_URL=https://baked.test\n"),
        (
            "index.html",
            r#"<!doctype html><html><head></head><body><script type="module">window.__backend = import.meta.env.VITE_BACKEND_URL;</script></body></html>"#,
        ),
    ]);
    let server = serve_with(&app, None, None).await;
    let html = server.text("/").await;
    let proxy = regex::Regex::new(r#"src="([^"]*html-proxy[^"]*)""#)
        .unwrap()
        .captures(&html)
        .unwrap_or_else(|| panic!("no html-proxy module in the page:\n{html}"))[1]
        .to_string();
    let module = server.text(&proxy).await;
    assert_eq!(
        first_statement(&module),
        "import \"/@oj/env.js\";",
        "{module}"
    );
    assert!(
        module.contains("globalThis.__oj_env.VITE_BACKEND_URL"),
        "{module}"
    );
    assert!(!module.contains("baked.test"), "{module}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_worker_module_reads_through_the_global() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"runtime-env-worker","private":true}"#,
        ),
        (".env", "VITE_BACKEND_URL=https://baked.test\n"),
        ("index.html", INDEX),
        (
            "src/main.js",
            "new Worker(new URL(\"./worker.js\", import.meta.url), { type: \"module\" });\n",
        ),
        (
            "src/worker.js",
            "self.postMessage(import.meta.env.VITE_BACKEND_URL);\n",
        ),
    ]);
    let server = serve_with(&app, None, None).await;
    let worker = server.text("/src/worker.js?worker_file&type=module").await;
    assert!(worker.contains("import \"/@oj/env.js\";"), "{worker}");
    assert!(
        worker.contains("globalThis.__oj_env.VITE_BACKEND_URL"),
        "{worker}"
    );
    assert!(!worker.contains("baked.test"), "{worker}");
}

// ---- review follow-ups ---------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_values_directory_created_after_boot_is_watched_once_it_exists() {
    let app = app();
    let dir = values_dir(&app).join("later");
    let file = dir.join("env.json");
    let server = serve_with(&app, Some(file.clone()), None).await;
    assert_eq!(status(&server).await["state"], "unbound");

    std::fs::create_dir_all(&dir).unwrap();
    let seen = settle(
        &server,
        || std::fs::write(&file, BOUND).unwrap(),
        |s| s["state"] == "bound",
    )
    .await;
    assert_eq!(seen["file"]["state"], "complete", "{seen}");
    assert!(server.text("/@oj/env.js").await.contains("/_port_8001"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_change_with_no_page_connected_reaches_the_next_page_to_connect() {
    let app = app();
    let server = serve_with(&app, None, None).await;
    let res = post(&server, r#"{"VITE_BACKEND_URL":"/_port_9000"}"#, &[]).await;
    assert_eq!(res.status, 200, "{}", res.body);
    // The page was still booting: its socket opens after the change.
    let listening = server.listen().await;
    let (mut socket, _) = server.ws(listening.addr, "/__ws", None).await;
    frame_matching(&mut socket, "the buffered reload", |f| {
        f["type"] == "full-reload" && f["reason"] == "runtime env"
    })
    .await;
    close(socket).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_pending_page_carries_the_client_so_the_binding_reloads_it() {
    let app = app();
    let file = values_dir(&app).join("env.json");
    std::fs::write(&file, PENDING).unwrap();
    let server = serve_with(&app, Some(file), None).await;
    let res = server.get("/").await;
    assert_eq!(res.status, 503);
    assert!(header(&res, "content-type").is_some_and(|t| t.starts_with("text/html")));
    assert!(
        res.body
            .contains(r#"<script type="module" src="/@oj/client.js">"#),
        "{}",
        res.body
    );
    assert!(res.body.contains("runtime env pending"), "{}", res.body);
    assert_eq!(server.get("/@oj/client.js").await.status, 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn under_a_nonce_the_pending_page_nonces_its_script() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"runtime-env-nonce","private":true}"#,
        ),
        ("oj.config.json", r#"{"html":{"cspNonce":"n0nce"}}"#),
        ("index.html", INDEX),
        ("src/main.js", MAIN),
    ]);
    let file = values_dir(&app).join("env.json");
    std::fs::write(&file, PENDING).unwrap();
    let server = serve_with(&app, Some(file), None).await;
    let res = server.get("/").await;
    assert_eq!(res.status, 503);
    assert!(
        res.body.contains(r#"src="/@oj/client.js" nonce="n0nce">"#),
        "{}",
        res.body
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_values_change_passes_the_editor_hmr_gate_without_a_flush() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"runtime-env-gate","private":true}"#,
        ),
        ("oj.config.json", r#"{"server":{"hmrGate":true}}"#),
        (".env", "VITE_BACKEND_URL=https://baked.test\n"),
        ("index.html", INDEX),
        ("src/main.js", MAIN),
    ]);
    let file = values_dir(&app).join("env.json");
    std::fs::write(&file, PENDING).unwrap();
    let server = serve_with(&app, Some(file.clone()), None).await;
    let listening = server.listen().await;
    let (mut socket, _) = server.ws(listening.addr, "/__ws", None).await;

    settle(
        &server,
        || std::fs::write(&file, BOUND).unwrap(),
        |s| s["state"] == "bound",
    )
    .await;
    frame_matching(&mut socket, "the reload, with no flush", |f| {
        f["type"] == "full-reload" && f["reason"] == "runtime env"
    })
    .await;
    let gate: serde_json::Value = serde_json::from_str(&server.text("/__hmr_gate").await).unwrap();
    assert_eq!(
        gate["count"], 0,
        "the values file is never held by the gate: {gate}"
    );
    close(socket).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plugins_html_transform_sees_the_values_in_force() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"runtime-env-plugin","version":"1.0.0"}"#,
        ),
        (
            "vite.config.mjs",
            r#"export default { plugins: [{ name: "mark", transformIndexHtml(h) { return h.replace("</head>", "<meta name=\"mark\"></head>"); } }] };"#,
        ),
        (".env", "VITE_BACKEND_URL=https://baked.test\n"),
        ("index.html", INDEX),
        ("src/main.js", MAIN),
    ]);
    let server = serve_with(&app, None, None).await;
    let html = server.text("/").await;
    assert!(
        html.contains(r#"<meta name="mark">"#),
        "the plugin ran:\n{html}"
    );
    assert!(html.contains(r#"content="https://baked.test""#), "{html}");
    let res = post(&server, r#"{"VITE_BACKEND_URL":"/_port_8001"}"#, &[]).await;
    assert_eq!(res.status, 200, "{}", res.body);
    let html = server.text("/").await;
    assert!(
        html.contains(r#"content="/_port_8001""#),
        "%KEY% through the plugin host follows the value in force:\n{html}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_module_reading_an_unset_key_imports_the_env_module() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"runtime-env-unset","private":true}"#,
        ),
        (".env", "VITE_BACKEND_URL=https://baked.test\n"),
        ("index.html", INDEX),
        (
            "src/main.js",
            "export const dsn = import.meta.env.VITE_SENTRY_DSN;\n",
        ),
    ]);
    let server = serve_with(&app, None, None).await;
    let main = server.text("/src/main.js").await;
    assert_eq!(first_statement(&main), "import \"/@oj/env.js\";", "{main}");
    assert!(
        main.contains("globalThis.__oj_env.VITE_SENTRY_DSN"),
        "{main}"
    );
}
