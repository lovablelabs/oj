// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A restart is checked before it replaces the running server, as Vite's
//! restartServer creates the new server first: a config that would not boot
//! keeps the running one and logs "server restart failed". The check
//! evaluates the config fresh, as Vite resolves it again, and a config edit
//! is a restart only: no hot update for it, nothing to the browser.

use std::time::Duration;

use crate::{require_fixture, restart_failures, take_restarts_for, App, INDEX_HTML};

/// Starts capturing restart failures, before a test triggers one.
fn capture_failures() {
    restart_failures("");
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(tokio::time::Instant::now() < deadline, "never: {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn js(path: &std::path::Path) -> String {
    serde_json::to_string(&path.display().to_string()).unwrap()
}

/// Vite's restart loops while requests are pending: a broken edit saved while
/// a passing check still evaluates is checked too, and keeps the server.
#[tokio::test(flavor = "multi_thread")]
async fn an_edit_during_a_passing_check_is_checked_before_the_restart() {
    capture_failures();
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"restart-pending","private":true,"type":"module"}"#,
        ),
        ("index.html", INDEX_HTML),
        ("src/main.js", "window.__ok = 1;\n"),
        ("vite.config.mjs", "export default {};\n"),
    ]);
    let server = app.serve().await;
    server.text("/src/main.js").await;
    let (started, release) = (app.root.join("started"), app.root.join("release"));
    let slow = format!(
        r#"import fs from "node:fs";
fs.writeFileSync({started}, "");
while (!fs.existsSync({release})) await new Promise((r) => setTimeout(r, 20));
export default {{}};
"#,
        started = js(&started),
        release = js(&release),
    );
    app.write(&[("vite.config.mjs", &slow)]);
    until("the slow check started", || started.exists()).await;
    app.write(&[("vite.config.mjs", "throw new Error(\"broken\");\n")]);
    // The broken edit's request lands while the first check waits.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    std::fs::write(&release, "").unwrap();
    let root = app.root.display().to_string();
    until("the broken edit's check failed", || {
        !restart_failures(&root).is_empty()
    })
    .await;
    assert_eq!(take_restarts_for(&app.root), Vec::<String>::new());
    assert!(server.text("/src/main.js").await.contains("__ok"));
}

/// A plugin whose config hook validates `data.yaml` (a file the config cache
/// does not track) and a `/__restart` route calling server.restart().
fn validating_config(app_root: &std::path::Path, marker: &str, extra: &str) -> String {
    format!(
        r#"import fs from "node:fs";
export default {{
  plugins: [{{
    name: "validates-data",
    config() {{
      if (fs.readFileSync({data}, "utf8").includes("bad")) throw new Error("{marker}");
    }},
    configureServer(server) {{
      server.middlewares.use((req, res, next) => {{
        if (req.url !== "/__restart") return next();
        server.restart();
        res.end("restarting");
      }});
    }},
  }}],
}};
{extra}"#,
        data = js(&app_root.join("data.yaml")),
    )
}

fn validating_app(name: &str) -> App {
    let app = App::new(&[
        (
            "package.json",
            &format!(r#"{{"name":"{name}","private":true,"type":"module"}}"#),
        ),
        ("index.html", INDEX_HTML),
        ("src/main.js", "window.__ok = 1;\n"),
        ("data.yaml", "good\n"),
    ]);
    app.link_fixture("vite");
    app
}

/// server.restart() after a plugin's own input broke: the check resolves the
/// config again (Vite's restartServer does), fails, and keeps the server.
#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_restart_over_a_broken_untracked_input_keeps_serving() {
    capture_failures();
    require_fixture!("vite");
    let app = validating_app("restart-untracked");
    let marker = format!("bad-data-{}", std::process::id());
    app.write(&[(
        "vite.config.mjs",
        &validating_config(&app.root, &marker, ""),
    )]);
    let server = app.serve().await;
    app.write(&[("data.yaml", "bad\n")]);
    server.text("/__restart").await;
    until("the restart check failed", || {
        !restart_failures(&marker).is_empty()
    })
    .await;
    assert_eq!(take_restarts_for(&app.root), Vec::<String>::new());
    assert!(server.text("/src/main.js").await.contains("__ok"));
}

/// A failed check never sticks: once the missing input is fixed, the next
/// restart of the same config text passes.
#[tokio::test(flavor = "multi_thread")]
async fn a_fixed_untracked_input_lets_the_same_config_restart() {
    capture_failures();
    require_fixture!("vite");
    let app = validating_app("restart-unstick");
    let marker = format!("unstick-data-{}", std::process::id());
    app.write(&[(
        "vite.config.mjs",
        &validating_config(&app.root, &marker, ""),
    )]);
    let server = app.serve().await;
    app.write(&[("data.yaml", "bad\n")]);
    app.write(&[(
        "vite.config.mjs",
        &validating_config(&app.root, &marker, "// edited\n"),
    )]);
    until("the edit's check failed", || {
        !restart_failures(&marker).is_empty()
    })
    .await;
    app.write(&[("data.yaml", "good\n")]);
    server.text("/__restart").await;
    until("the same config restarted", || {
        // A plugin's restart names no file: claimed here before any
        // server's drop fails on it.
        let mut all = crate::RESTARTS.lock().unwrap();
        let at = all
            .iter()
            .position(|r| r == "plugin requested server.restart()");
        at.map(|at| all.remove(at)).is_some()
    })
    .await;
    assert_eq!(restart_failures(&marker).len(), 1, "it failed again");
}

/// Vite's handleHMRUpdate ends at a config change once it restarts: the
/// config's watchChange runs, its hot update hooks do not, and the browser
/// gets nothing for it, while an app file of the same batch gets its HMR.
#[tokio::test(flavor = "multi_thread")]
async fn a_config_edit_is_a_restart_only() {
    capture_failures();
    for broken in [true, false] {
        let log_dir = tempfile::tempdir().unwrap();
        let log = log_dir.path().join("hooks.log");
        let app = App::new(&[
            (
                "package.json",
                r#"{"name":"config-edit-no-hmr","private":true,"type":"module"}"#,
            ),
            ("index.html", INDEX_HTML),
            ("src/main.js", "window.__ok = 1;\n"),
        ]);
        let config = app.root.join("vite.config.mjs");
        let src = format!(
            r#"import fs from "node:fs";
const log = (line) => fs.appendFileSync({log}, line + "\n");
const base = (id) => id.split("/").pop();
export default {{
  plugins: [{{
    name: "hooks",
    buildStart() {{ this.addWatchFile({config}); }},
    watchChange(id) {{ log("watchChange " + base(id)); }},
    hotUpdate({{ file }}) {{ log("hotUpdate " + this.environment.name + " " + base(file)); }},
    handleHotUpdate({{ file }}) {{ log("handleHotUpdate " + base(file)); }},
  }}],
}};
"#,
            log = js(&log),
            config = js(&config),
        );
        app.write(&[("vite.config.mjs", &src)]);
        let server = app.serve().await;
        assert!(server.text("/src/main.js").await.contains("__ok"));
        let mut frames = server.built.reload_tx.subscribe();
        let edited = if broken {
            format!("{src}throw new Error(\"broken\");\n")
        } else {
            format!("{src}// edited\n")
        };
        app.write(&[
            ("vite.config.mjs", &edited),
            ("src/main.js", "window.__ok = 2;\n"),
        ]);
        let read = || std::fs::read_to_string(&log).unwrap_or_default();
        until("the app file's hot update", || {
            read().contains("hotUpdate client main.js")
        })
        .await;
        until("the config's watchChange", || {
            read().contains("watchChange vite.config.mjs")
        })
        .await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        let text = read();
        assert!(
            !text
                .lines()
                .any(|l| l.contains("otUpdate") && l.contains("vite.config")),
            "a hot update hook ran for the config (broken {broken}):\n{text}"
        );
        let mut sent = Vec::new();
        while let Ok(frame) = frames.try_recv() {
            sent.push(frame);
        }
        assert!(
            sent.iter().any(|f| f.contains("main.js")),
            "the app file got no HMR (broken {broken}): {sent:?}"
        );
        assert!(
            !sent.iter().any(|f| f.contains("vite.config")),
            "the browser got a frame for the config (broken {broken}): {sent:?}"
        );
        let root = app.root.display().to_string();
        if broken {
            until("the broken config's check failed", || {
                !restart_failures(&root).is_empty()
            })
            .await;
        } else {
            until("the valid config restarted", || {
                !take_restarts_for(&app.root).is_empty()
            })
            .await;
        }
    }
}
