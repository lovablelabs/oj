// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Runtime env keys (`OJ_RUNTIME_ENV_KEYS`): a module that reads one compiles
//! to a read of the global `/@oj/env.js` defines and imports that module
//! first; the values come from a local `POST /@oj/env`, a change reloads the
//! pages, and the compiled modules never change across a rebind.

use std::path::PathBuf;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use oj_server::RuntimeEnvConfig;

use crate::listen::{close, frame_matching, next_text_within};
use crate::{App, Response, Server};

const INDEX: &str = r#"<!doctype html><html><head><meta name="backend" content="%VITE_BACKEND_URL%"></head><body><script type="module" src="/src/main.js"></script></body></html>"#;
const MAIN: &str = "export const backend = import.meta.env.VITE_BACKEND_URL;\nexport const key = import.meta.env.VITE_BACKEND_KEY;\nexport const mode = import.meta.env.MODE;\n";
const PLAIN: &str = "export const title = import.meta.env.VITE_TITLE;\n";
/// Every key, set.
const BOUND: &str = r#"{"VITE_BACKEND_URL":"/_port_8001","VITE_BACKEND_KEY":"sb_publishable_x"}"#;

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

fn keys() -> Vec<String> {
    vec!["VITE_BACKEND_URL".into(), "VITE_BACKEND_KEY".into()]
}

async fn serve_with(app: &App, ready: Option<PathBuf>) -> Server {
    let mut dev = app.dev();
    dev.runtime_env = Some(RuntimeEnvConfig {
        keys: keys(),
        ready_file: ready,
    });
    let server = crate::serve(dev).await;
    // As `bind_listener` does: the ready file is a listening server's.
    server.built.runtime_env_serving();
    server
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

fn header<'a>(res: &'a Response, name: &str) -> Option<&'a str> {
    res.headers.get(name).and_then(|v| v.to_str().ok())
}

fn first_statement(code: &str) -> &str {
    code.trim_start().split_inclusive(';').next().unwrap_or("")
}

#[tokio::test(flavor = "multi_thread")]
async fn redirected_modules_import_the_env_module_and_serve_the_literal_until_bound() {
    let app = app();
    let server = serve_with(&app, None).await;

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
    assert_eq!(status["module"], "/@oj/env.js");
    assert_eq!(
        status["keys"],
        serde_json::json!([
            {"key": "VITE_BACKEND_URL", "value": "https://baked.test", "source": "fallback"},
            {"key": "VITE_BACKEND_KEY", "value": null, "source": null},
        ])
    );
    assert_eq!(status["readyFile"], serde_json::Value::Null);
    assert!(
        status.get("file").is_none(),
        "the status names no file: {status}"
    );
}

// With keys named and nothing set, a document is served as it is for an app
// with no runtime keys: no key waits for a value, each reads its literal.
#[tokio::test(flavor = "multi_thread")]
async fn with_nothing_set_documents_are_served_as_without_runtime_keys() {
    let app = app();
    let server = serve_with(&app, None).await;
    let plain_app = self::app();
    let plain = plain_app.serve().await;

    for path in ["/", "/index.html", "/some/route"] {
        let res = server.get(path).await;
        assert_eq!(res.status, 200, "{path}: {}", res.body);
        assert!(
            header(&res, "content-type").is_some_and(|t| t.starts_with("text/html")),
            "{path}"
        );
        assert_eq!(header(&res, "retry-after"), None, "{path}");
        assert!(
            res.body.contains(r#"content="https://baked.test""#),
            "{path}: {}",
            res.body
        );
        let without = plain.get(path).await;
        assert_eq!(without.status, 200, "{path}");
        assert_eq!(
            res.body, without.body,
            "{path}: the document an app without runtime keys gets"
        );
        assert_eq!(
            header(&res, "cache-control"),
            header(&without, "cache-control"),
            "{path}"
        );
    }
    assert_eq!(status(&server).await["state"], "unbound");

    // The same with some keys set and others not.
    let res = post(&server, r#"{"VITE_BACKEND_KEY":"k"}"#, &[]).await;
    assert_eq!(res.status, 200, "{}", res.body);
    assert_eq!(status(&server).await["state"], "partial");
    assert_eq!(server.get("/").await.status, 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn bound_values_reach_the_module_the_page_and_the_ready_file() {
    let app = app();
    let ready = app.root.join("ready");
    // One a previous run left: nothing is bound yet, so it goes at boot.
    std::fs::write(&ready, "1\n").unwrap();
    let server = serve_with(&app, Some(ready.clone())).await;
    assert!(!ready.exists(), "unbound at boot");

    let res = post(&server, r#"{"VITE_BACKEND_URL":"/_port_8001"}"#, &[]).await;
    assert_eq!(res.status, 200, "{}", res.body);
    assert!(!ready.exists(), "partial: one key has no value");

    let res = post(&server, BOUND, &[]).await;
    assert_eq!(res.status, 200, "{}", res.body);
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
    assert_eq!(status["generation"], 2);
    assert_eq!(status["keys"][0]["source"], "set");
    assert_eq!(status["keys"][1]["source"], "set");
    assert_eq!(status["readyFile"], ready.display().to_string());
    assert_eq!(std::fs::read_to_string(&ready).unwrap(), "1\n");

    // Unbound again: a key unset takes the ready file with it.
    let res = post(&server, r#"{"VITE_BACKEND_KEY":null}"#, &[]).await;
    assert_eq!(res.status, 200, "{}", res.body);
    assert!(!ready.exists(), "no longer bound");
    let res = post(&server, BOUND, &[]).await;
    assert_eq!(res.status, 200, "{}", res.body);
    assert!(ready.exists(), "bound again");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_local_post_sets_values_and_reloads_the_pages() {
    let app = app();
    let server = serve_with(&app, None).await;
    let listening = server.listen().await;
    let (mut socket, _) = server.ws(listening.addr, "/__ws", None).await;

    let res = post(&server, r#"{"VITE_BACKEND_URL":"/_port_9000"}"#, &[]).await;
    assert_eq!(res.status, 200, "{}", res.body);
    assert_eq!(header(&res, "cache-control"), Some("no-store"));
    let status: serde_json::Value = serde_json::from_str(&res.body).unwrap();
    assert_eq!(status["state"], "partial", "one of two keys set: {status}");
    assert_eq!(status["generation"], 1);
    assert_eq!(status["keys"][0]["value"], "/_port_9000");
    assert_eq!(status["keys"][0]["source"], "set");
    assert_eq!(status["keys"][1]["source"], serde_json::Value::Null);

    let frame = frame_matching(&mut socket, "the reload onto the new values", |f| {
        f["type"] == "full-reload" && f["reason"] == "runtime env"
    })
    .await;
    assert_eq!(frame["path"], "*");
    assert!(
        frame.get("triggeredBy").is_none(),
        "no file did it: {frame}"
    );

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
    let server = serve_with(&app, None).await;
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
    // The set values together stay under 64 KB.
    let big =
        serde_json::json!({ "VITE_BACKEND_URL": "/a", "VITE_BACKEND_KEY": "x".repeat(64 * 1024) });
    let res = post(&server, &big.to_string(), &[]).await;
    assert_eq!(res.status, 400);
    assert!(res.body.contains("larger than"), "{}", res.body);

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
async fn compiled_modules_are_stable_across_a_rebind() {
    let app = app();
    let server = serve_with(&app, None).await;
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
    let server = serve_with(&app, None).await;
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
    let server = serve_with(&app, None).await;
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
    let server = serve_with(&app, None).await;
    let worker = server.text("/src/worker.js?worker_file&type=module").await;
    assert!(worker.contains("import \"/@oj/env.js\";"), "{worker}");
    assert!(
        worker.contains("globalThis.__oj_env.VITE_BACKEND_URL"),
        "{worker}"
    );
    assert!(!worker.contains("baked.test"), "{worker}");
}

// ---- review follow-ups ---------------------------------------------------------------

// A change made with no page connected is kept for nobody: the next page to
// load reads the values in force, and a reload kept for its socket reloaded
// it for nothing. A page handed the old values just before the change, its
// socket not open yet, finds out by itself: the module it loaded asks for
// the generation in force.
#[tokio::test(flavor = "multi_thread")]
async fn a_change_with_no_page_connected_is_found_by_the_generation_not_a_kept_reload() {
    let app = app();
    let server = serve_with(&app, None).await;
    let before = server.text("/@oj/env.js").await;
    assert!(before.contains("g === \"0\""), "{before}");
    assert_eq!(server.text("/@oj/env.js?generation").await, "0");

    let res = post(&server, r#"{"VITE_BACKEND_URL":"/_port_9000"}"#, &[]).await;
    assert_eq!(res.status, 200, "{}", res.body);
    let res = server.get("/@oj/env.js?generation").await;
    assert_eq!(res.body, "1", "what the page handed generation 0 compares");
    assert_eq!(header(&res, "cache-control"), Some("no-store"));
    assert!(server.text("/@oj/env.js").await.contains("g === \"1\""));

    // A page loading now: its socket gets no reload.
    let listening = server.listen().await;
    let (mut socket, _) = server.ws(listening.addr, "/__ws", None).await;
    let mut frames = Vec::new();
    while let Some(text) = next_text_within(&mut socket, Duration::from_millis(700)).await {
        frames.push(text);
    }
    assert!(
        !frames.iter().any(|f| f.contains("full-reload")),
        "{frames:?}"
    );
    close(socket).await;
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
    let server = serve_with(&app, None).await;
    let listening = server.listen().await;
    let (mut socket, _) = server.ws(listening.addr, "/__ws", None).await;

    let res = post(&server, BOUND, &[]).await;
    assert_eq!(res.status, 200, "{}", res.body);
    frame_matching(&mut socket, "the reload, with no flush", |f| {
        f["type"] == "full-reload" && f["reason"] == "runtime env"
    })
    .await;
    let gate: serde_json::Value = serde_json::from_str(&server.text("/__hmr_gate").await).unwrap();
    assert_eq!(
        gate["count"], 0,
        "a values change is never held by the gate: {gate}"
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
    let server = serve_with(&app, None).await;
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
    let server = serve_with(&app, None).await;
    let main = server.text("/src/main.js").await;
    assert_eq!(first_statement(&main), "import \"/@oj/env.js\";", "{main}");
    assert!(
        main.contains("globalThis.__oj_env.VITE_SENTRY_DSN"),
        "{main}"
    );
}

// ---- second review ---------------------------------------------------------------------

// A caller on another machine: with `--host 0.0.0.0` a plain `curl` to the
// port has no Origin and no forwarded host, and set the values for every
// page. Refused by where its connection comes from; this machine's is not.
#[tokio::test(flavor = "multi_thread")]
async fn the_control_endpoint_refuses_a_caller_on_another_machine() {
    let app = app();
    let server = serve_with(&app, None).await;
    let body = r#"{"VITE_BACKEND_URL":"/evil"}"#;
    let from = |ip: &str| oj_server::tls::ClientConn {
        addr: std::net::SocketAddr::new(ip.parse().unwrap(), 40000),
        encrypted: false,
    };
    for remote in ["192.0.2.20", "::ffff:203.0.113.5", "2001:db8::1"] {
        let res = oj_server::tls::with_client_conn(from(remote), post(&server, body, &[])).await;
        assert_eq!(res.status, 403, "{remote}: {}", res.body);
        let res = oj_server::tls::with_client_conn(from(remote), server.get("/@oj/env")).await;
        assert_eq!(res.status, 403, "{remote} may not read them either");
    }
    assert!(!server.text("/@oj/env.js").await.contains("/evil"));

    // A browser on this machine, the page's own same-origin request included.
    let res = oj_server::tls::with_client_conn(
        from("127.0.0.1"),
        server.request(
            Request::get("/@oj/env")
                .header("sec-fetch-site", "same-origin")
                .body(Body::empty())
                .unwrap(),
        ),
    )
    .await;
    assert_eq!(res.status, 403);

    for local in ["127.0.0.1", "::1", "::ffff:127.0.0.1"] {
        let res = oj_server::tls::with_client_conn(from(local), post(&server, body, &[])).await;
        assert_eq!(res.status, 200, "{local}: {}", res.body);
    }
    assert!(server.text("/@oj/env.js").await.contains("/evil"));
}

// A classic worker's file: `importScripts` of the script form, no static
// import, which a classic worker cannot hold.
#[tokio::test(flavor = "multi_thread")]
async fn a_classic_worker_loads_the_values_without_an_import() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"runtime-env-classic-worker","private":true}"#,
        ),
        (".env", "VITE_BACKEND_URL=https://baked.test\n"),
        ("index.html", INDEX),
        (
            "src/main.js",
            "new Worker(new URL(\"./worker.js\", import.meta.url));\n",
        ),
        (
            "src/worker.js",
            "self.postMessage(import.meta.env.VITE_BACKEND_URL);\n",
        ),
    ]);
    let server = serve_with(&app, None).await;
    let worker = server.text("/src/worker.js?worker_file&type=classic").await;
    assert!(
        worker.contains("importScripts(\"/@oj/env.js?classic\");"),
        "{worker}"
    );
    assert!(!worker.contains("import \""), "{worker}");
    assert!(
        worker.contains("globalThis.__oj_env.VITE_BACKEND_URL"),
        "{worker}"
    );
    let script = server.text("/@oj/env.js?classic").await;
    assert!(!script.contains("export"), "{script}");
    assert!(script.contains("\"https://baked.test\""), "{script}");
    // A worker whose type oj cannot read gets the script inline, which a
    // module worker runs too.
    let unknown = server.text("/src/worker.js?worker_file&type=ignore").await;
    assert!(
        !unknown.contains("importScripts(\"/@oj/env.js"),
        "{unknown}"
    );
    assert!(!unknown.contains("import \"/@oj/env.js\""), "{unknown}");
    assert!(
        unknown.contains("{ const env = {") && unknown.contains("\"https://baked.test\""),
        "{unknown}"
    );
    // The module worker's file is as it was.
    let module = server.text("/src/worker.js?worker_file&type=module").await;
    assert!(module.contains("import \"/@oj/env.js\";"), "{module}");
}

// ---- more tests ------------------------------------------------------------------------

// Under a base: the generation, the classic script and a classic worker's
// file all answer and name each other under it.
#[tokio::test(flavor = "multi_thread")]
async fn under_a_base_the_generation_and_the_classic_script_answer_under_it() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"runtime-env-base-worker","private":true}"#,
        ),
        ("oj.config.json", r#"{"base":"/app/"}"#),
        (".env", "VITE_BACKEND_URL=https://baked.test\n"),
        ("index.html", INDEX),
        (
            "src/main.js",
            "new Worker(new URL(\"./worker.js\", import.meta.url));\n",
        ),
        (
            "src/worker.js",
            "self.postMessage(import.meta.env.VITE_BACKEND_URL);\n",
        ),
    ]);
    let server = serve_with(&app, None).await;
    assert_eq!(server.text("/app/@oj/env.js?generation").await, "0");
    let module = server.text("/app/@oj/env.js").await;
    assert!(
        module.contains("fetch(\"/app/@oj/env.js?generation\""),
        "{module}"
    );
    let script = server.text("/app/@oj/env.js?classic").await;
    assert!(
        script.starts_with("{\nconst env = {") && !script.contains("export"),
        "{script}"
    );
    let worker = server
        .text("/app/src/worker.js?worker_file&type=classic")
        .await;
    assert!(
        worker.contains("importScripts(\"/app/@oj/env.js?classic\");"),
        "{worker}"
    );
    assert!(!worker.contains("import \"/app/@oj/env.js\""), "{worker}");
    let res = post(&server, r#"{"VITE_BACKEND_URL":"/_port_9000"}"#, &[]).await;
    assert_eq!(res.status, 200, "{}", res.body);
    assert_eq!(server.text("/app/@oj/env.js?generation").await, "1");
}

// A request a proxy on this machine relays comes over a local connection:
// what the proxy adds for the caller it stands for refuses it.
#[tokio::test(flavor = "multi_thread")]
async fn the_control_endpoint_refuses_what_a_local_proxy_relays() {
    let app = app();
    let server = serve_with(&app, None).await;
    let body = r#"{"VITE_BACKEND_URL":"/evil"}"#;
    let local = oj_server::tls::ClientConn {
        addr: "127.0.0.1:40000".parse().unwrap(),
        encrypted: false,
    };
    for (name, value) in [
        ("x-forwarded-for", "203.0.113.5"),
        ("forwarded", "for=203.0.113.5;proto=https"),
        ("x-real-ip", "203.0.113.5"),
        ("via", "1.1 tunnel"),
        ("sec-fetch-site", "cross-site"),
    ] {
        let res =
            oj_server::tls::with_client_conn(local, post(&server, body, &[(name, value)])).await;
        assert_eq!(res.status, 403, "{name}: {}", res.body);
        assert_eq!(header(&res, "cache-control"), Some("no-store"));
    }
    assert!(!server.text("/@oj/env.js").await.contains("/evil"));
    // The module and its generation are the page's to read, whoever asks.
    let res = oj_server::tls::with_client_conn(
        oj_server::tls::ClientConn {
            addr: "203.0.113.5:40000".parse().unwrap(),
            encrypted: false,
        },
        server.request(
            Request::get("/@oj/env.js?generation")
                .header("sec-fetch-site", "same-origin")
                .body(Body::empty())
                .unwrap(),
        ),
    )
    .await;
    assert_eq!((res.status, res.body.as_str()), (200, "0"));
}

// A set that changes nothing reloads nobody; one that does reloads the
// pages once.
#[tokio::test(flavor = "multi_thread")]
async fn a_set_of_the_value_in_force_reloads_nobody() {
    let app = app();
    let server = serve_with(&app, None).await;
    let listening = server.listen().await;
    let (mut socket, _) = server.ws(listening.addr, "/__ws", None).await;
    let body = r#"{"VITE_BACKEND_URL":"/_port_9000"}"#;
    assert_eq!(post(&server, body, &[]).await.status, 200);
    frame_matching(&mut socket, "the reload", |f| f["type"] == "full-reload").await;
    let res = post(&server, body, &[]).await;
    assert_eq!(res.status, 200);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&res.body).unwrap()["generation"],
        1
    );
    let mut frames = Vec::new();
    while let Some(text) = next_text_within(&mut socket, Duration::from_millis(700)).await {
        frames.push(text);
    }
    assert!(
        !frames.iter().any(|f| f.contains("full-reload")),
        "{frames:?}"
    );
    close(socket).await;
}

// Both forms of the module are the server's own pages' and workers' to
// load: another site's `<script src>` of the classic form would run it and
// read the global it installs.
#[tokio::test(flavor = "multi_thread")]
async fn the_env_module_is_not_another_origins_to_load() {
    let app = app();
    let server = serve_with(&app, None).await;
    assert_eq!(post(&server, BOUND, &[]).await.status, 200);
    for url in ["/@oj/env.js", "/@oj/env.js?classic"] {
        let res = server.get(url).await;
        assert_eq!(res.status, 200, "{url}");
        assert_eq!(
            header(&res, "cross-origin-resource-policy"),
            Some("same-origin"),
            "{url}"
        );
    }
}

// A compiled file a page loads as a classic script (`<script src>` with no
// `type="module"`), or registers as a service worker, cannot hold the
// import a compile puts first: it gets the values inline, and its tag
// follows them.
#[tokio::test(flavor = "multi_thread")]
async fn a_classic_script_gets_the_values_inline_not_an_import() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"runtime-env-classic-script","private":true}"#,
        ),
        ("index.html", INDEX),
        ("src/main.js", MAIN),
        (
            "src/sw.js",
            "self.backend = import.meta.env.VITE_BACKEND_URL;\n",
        ),
    ]);
    let server = serve_with(&app, None).await;
    let get = |headers: Vec<(&'static str, String)>| {
        let mut req = axum::http::Request::get("/src/sw.js");
        for (name, value) in headers {
            req = req.header(name, value);
        }
        server.request(req.body(axum::body::Body::empty()).unwrap())
    };
    let classic = |dest: &'static str, mode: &'static str| {
        vec![
            ("sec-fetch-dest", dest.to_string()),
            ("sec-fetch-mode", mode.to_string()),
        ]
    };
    let module = get(classic("script", "cors")).await;
    assert!(
        module.body.starts_with("import \"/@oj/env.js\";"),
        "{}",
        module.body
    );
    for (dest, mode) in [("script", "no-cors"), ("serviceworker", "same-origin")] {
        let res = get(classic(dest, mode)).await;
        assert_eq!(res.status, 200, "{dest}");
        assert!(!res.body.contains("import \""), "{dest}: {}", res.body);
        assert!(res.body.contains("\"__oj_env\""), "{dest}: {}", res.body);
        assert!(
            res.body.contains("globalThis.__oj_env.VITE_BACKEND_URL"),
            "{dest}: {}",
            res.body
        );
        assert_eq!(
            res.body.lines().count(),
            module.body.lines().count(),
            "the map stands"
        );
        assert_ne!(header(&res, "etag"), header(&module, "etag"));
    }
    // Its tag: not modified while the values stand, served again once they move.
    let first = get(classic("script", "no-cors")).await;
    let etag = header(&first, "etag").unwrap().to_string();
    let mut again = classic("script", "no-cors");
    again.push(("if-none-match", etag.clone()));
    assert_eq!(get(again.clone()).await.status, 304);
    let res = post(&server, r#"{"VITE_BACKEND_URL":"/_port_9"}"#, &[]).await;
    assert_eq!(res.status, 200, "{}", res.body);
    let moved = get(again).await;
    assert_eq!(moved.status, 200);
    assert!(moved.body.contains("\"/_port_9\""), "{}", moved.body);
}

// Under a base, with nothing set: documents, modules, the env module and the
// control endpoint all answer.
#[tokio::test(flavor = "multi_thread")]
async fn under_a_base_documents_are_served_with_nothing_set() {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"runtime-env-base-documents","private":true}"#,
        ),
        ("oj.config.json", r#"{"base":"/app/"}"#),
        ("index.html", INDEX),
        ("src/main.js", MAIN),
    ]);
    let server = serve_with(&app, None).await;
    for served in [
        "/app/",
        "/app/index.html",
        "/app/some/route",
        "/app/src/main.js",
        "/app/@oj/env.js",
        "/app/@oj/env.js?generation",
        "/app/@oj/env",
    ] {
        assert_eq!(server.get(served).await.status, 200, "{served}");
    }
}
