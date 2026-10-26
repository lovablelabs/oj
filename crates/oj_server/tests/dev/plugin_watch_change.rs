// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Vite's watcher calls a plugin's `watchChange` on every change, before the
//! module graph invalidation and before HMR (`onFileChange`, then
//! `onHMRUpdate` in server/index.ts). `server.hmr: false` skips only the HMR
//! part (`hotUpdate`), and the editor gate only holds it: `watchChange` still
//! runs once per change (on the client environment: another environment's
//! container runs it only for a plugin that asks per environment), never
//! again at the flush, and with HMR on it lands before `hotUpdate`. Its event
//! is `create`, `update` or `delete`.

use std::time::Duration;

use axum::body::Body;
use axum::http::Request;

use crate::plugin_middleware::middleware_ready;
use crate::{App, Server};

/// A plugin recording its watcher hooks, readable at `/__log`.
fn config(server: &str) -> String {
    format!(
        r#"const log = [];
export default {{
  server: {server},
  plugins: [{{
    name: "recorder",
    watchChange(id, change) {{ log.push(`watch ${{this.environment.name}} ${{change.event}} ${{id}}`); }},
    hotUpdate(ctx) {{ log.push(`hot ${{this.environment.name}} ${{ctx.type}} ${{ctx.file}}`); }},
    configureServer(server) {{
      server.middlewares.use((req, res, next) => {{
        if (req.url !== "/__log") return next();
        res.setHeader("content-type", "application/json");
        res.end(JSON.stringify(log));
      }});
    }},
  }}],
}};
"#
    )
}

/// `gated`: the editor gate on (`server.hmrGate` in oj.config.json).
async fn boot(server_config: &str, gated: bool) -> (App, Server) {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"watch-change","version":"1.0.0"}"#,
        ),
        ("vite.config.mjs", &config(server_config)),
        ("src/main.js", "document.title = \"v1\";\n"),
        ("index.html", crate::INDEX_HTML),
    ]);
    if gated {
        app.write(&[("oj.config.json", r#"{"server":{"hmrGate":true}}"#)]);
    }
    let server = app.serve().await;
    middleware_ready(&server).await;
    // In the graph, so the edit is an `update` even where the watcher still
    // flags the file as freshly created.
    server.text("/src/main.js").await;
    (app, server)
}

async fn log(server: &Server) -> Vec<String> {
    serde_json::from_str(&server.text("/__log").await).unwrap()
}

/// The log once `done` holds of it.
async fn log_until(server: &Server, what: &str, done: impl Fn(&[String]) -> bool) -> Vec<String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let entries = log(server).await;
        if done(&entries) {
            return entries;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{what}; log: {entries:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn count(entries: &[String], prefix: &str, file: &str) -> usize {
    entries
        .iter()
        .filter(|e| e.starts_with(prefix) && e.ends_with(file))
        .count()
}

const MAIN: &str = "/src/main.js";

#[tokio::test(flavor = "multi_thread")]
async fn hmr_off_still_calls_watch_change_but_not_hot_update() {
    let (app, server) = boot("{ hmr: false }", false).await;
    app.write(&[("src/main.js", "document.title = \"v2\";\n")]);
    log_until(&server, "watchChange never ran with hmr: false", |l| {
        count(l, "watch client update ", MAIN) == 1
    })
    .await;
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let entries = log(&server).await;
    assert_eq!(
        count(&entries, "watch client update ", MAIN),
        1,
        "once: {entries:?}"
    );
    assert_eq!(
        count(&entries, "watch ssr update ", MAIN),
        0,
        "not again in the ssr environment: {entries:?}"
    );
    assert_eq!(
        count(&entries, "hot ", MAIN),
        0,
        "no hotUpdate: {entries:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn gated_edit_calls_watch_change_once_before_the_flush() {
    let (app, server) = boot("{}", true).await;
    app.write(&[("src/main.js", "document.title = \"v2\";\n")]);
    log_until(&server, "watchChange waited for the flush", |l| {
        count(l, "watch client update ", MAIN) == 1
    })
    .await;
    let gate: serde_json::Value = serde_json::from_str(&server.text("/__hmr_gate").await).unwrap();
    assert!(
        gate["enabled"] == true && gate["count"].as_u64().is_some_and(|c| c >= 1),
        "the edit is held: {gate}"
    );
    let res = server
        .request(Request::post("/__hmr_flush").body(Body::empty()).unwrap())
        .await;
    assert_eq!(res.status, 200, "flush:\n{}", res.body);
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let entries = log(&server).await;
    assert_eq!(
        count(&entries, "watch client update ", MAIN),
        1,
        "the flush must not call watchChange again: {entries:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn hmr_on_calls_watch_change_before_hot_update() {
    let (app, server) = boot("{}", false).await;
    app.write(&[("src/main.js", "document.title = \"v2\";\n")]);
    let entries = log_until(&server, "hotUpdate never ran", |l| {
        count(l, "hot client update ", MAIN) == 1
    })
    .await;
    let watch = entries
        .iter()
        .position(|e| e.starts_with("watch client update ") && e.ends_with(MAIN));
    let hot = entries
        .iter()
        .position(|e| e.starts_with("hot client update ") && e.ends_with(MAIN));
    assert!(
        watch.is_some() && watch < hot,
        "watchChange before hotUpdate: {entries:?}"
    );
    assert_eq!(
        count(&entries, "watch client update ", MAIN),
        1,
        "once: {entries:?}"
    );
}

/// Vite restarts on a config or `.env` change from `handleHMRUpdate`, which
/// `server.hmr: false` skips: the change still reaches `watchChange`.
#[tokio::test(flavor = "multi_thread")]
async fn hmr_off_config_and_env_edits_never_restart() {
    let (app, server) = boot("{ hmr: false }", false).await;
    app.write(&[(".env", "VITE_X=1\n")]);
    log_until(&server, "the .env edit never reached watchChange", |l| {
        count(l, "watch client ", "/.env") >= 1
    })
    .await;
    let config = format!("{}// edited\n", config("{ hmr: false }"));
    app.write(&[("vite.config.mjs", &config)]);
    log_until(&server, "the config edit never reached watchChange", |l| {
        count(l, "watch client update ", "/vite.config.mjs") >= 1
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let restarts = crate::take_restarts_for(&app.root);
    assert!(
        restarts.is_empty(),
        "restarted with hmr: false: {restarts:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_change_reports_create_and_delete() {
    let (app, server) = boot("{ hmr: false }", false).await;
    app.write(&[("src/added.js", "export {};\n")]);
    log_until(&server, "no create event", |l| {
        count(l, "watch client create ", "/src/added.js") == 1
    })
    .await;
    std::fs::remove_file(app.root.join("src/added.js")).unwrap();
    log_until(&server, "no delete event", |l| {
        count(l, "watch client delete ", "/src/added.js") == 1
    })
    .await;
}

/// chokidar reports `add` for a path again after its `unlink`: a file created,
/// deleted and created again is a create the second time, not an update.
#[tokio::test(flavor = "multi_thread")]
async fn watch_change_reports_create_again_after_a_delete() {
    let (app, server) = boot("{ hmr: false }", false).await;
    app.write(&[("src/again.js", "export {};\n")]);
    log_until(&server, "no first create event", |l| {
        count(l, "watch client create ", "/src/again.js") == 1
    })
    .await;
    std::fs::remove_file(app.root.join("src/again.js")).unwrap();
    log_until(&server, "no delete event", |l| {
        count(l, "watch client delete ", "/src/again.js") == 1
    })
    .await;
    app.write(&[("src/again.js", "export const x = 1;\n")]);
    let entries = log_until(&server, "no create event after the delete", |l| {
        count(l, "watch client create ", "/src/again.js") == 2
    })
    .await;
    assert_eq!(
        count(&entries, "watch client update ", "/src/again.js"),
        0,
        "the re-creation is no update: {entries:?}"
    );
}
