// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Vite's watcher handles each change event on its own (server/index.ts:
//! `watcher.on('change', file => onFileChange(file))`, never awaited), so a
//! burst of changes runs its `hotUpdate` hooks concurrently. Plugins rely on
//! that to collapse a burst: each call takes a ticket, awaits a short timer,
//! and only the call still holding the newest ticket does the work. Called
//! one file at a time, no newer ticket ever arrives during the wait and every
//! file does the work.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use crate::plugin_middleware::middleware_ready;
use crate::{require_fixture, App, INDEX_HTML};

const FILES: usize = 40;

/// A plugin that debounces a burst the way translation plugins do. Each line
/// of the log is `call <tag>` or `rebuild <tag>`; the tag names the plugin
/// instance and the environment, which each keep their own ticket.
const DEBOUNCED: &str = r#"
const instance = Math.random().toString(36).slice(2);
const tickets = new Map();
const debounced = {
  name: "debounced-rebuild",
  async hotUpdate({ file }) {
    if (!file.includes("/messages/")) return;
    const tag = instance + ":" + ((this && this.environment && this.environment.name) || "default");
    const ticket = (tickets.get(tag) ?? 0) + 1;
    tickets.set(tag, ticket);
    fs.appendFileSync(__LOG__, "call " + tag + "\n");
    await new Promise((r) => setTimeout(r, 200));
    if (tickets.get(tag) !== ticket) return [];
    fs.appendFileSync(__LOG__, "rebuild " + tag + "\n");
    return [];
  },
};
"#;

fn message_files() -> Vec<(String, String)> {
    (0..FILES)
        .map(|i| {
            (
                format!("src/messages/m{i}.js"),
                format!("export const m{i} = \"v{i}\";\n"),
            )
        })
        .collect()
}

/// `(calls, rebuilds)` per tag, once every tag with calls has seen `calls`
/// of them and the last call's timer had time to fire.
async fn settle(log: &Path, calls: usize) -> HashMap<String, (usize, usize)> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let counts = counts(log);
        if !counts.is_empty() && counts.values().all(|(c, _)| *c >= calls) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "hotUpdate calls never all arrived: {counts:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Serial calls would still be rebuilding one by one, 200 ms each.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    counts(log)
}

fn counts(log: &Path) -> HashMap<String, (usize, usize)> {
    let mut counts: HashMap<String, (usize, usize)> = HashMap::new();
    for line in std::fs::read_to_string(log).unwrap_or_default().lines() {
        match line.split_once(' ') {
            Some(("call", tag)) => counts.entry(tag.to_string()).or_default().0 += 1,
            Some(("rebuild", tag)) => counts.entry(tag.to_string()).or_default().1 += 1,
            _ => {}
        }
    }
    counts
}

fn assert_one_rebuild_each(counts: &HashMap<String, (usize, usize)>) {
    for (tag, (calls, rebuilds)) in counts {
        assert_eq!(
            (*calls, *rebuilds),
            (FILES, 1),
            "{tag}: a {FILES}-file burst must rebuild once: {counts:?}"
        );
    }
}

/// The log lives outside the app root so the watcher never sees it.
fn log_path(dir: &tempfile::TempDir) -> (std::path::PathBuf, String) {
    let log = dir.path().canonicalize().unwrap().join("hot.log");
    let literal = serde_json::to_string(&log.display().to_string()).unwrap();
    (log, literal)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_burst_of_edits_runs_hot_update_hooks_concurrently() {
    let log_dir = tempfile::tempdir().unwrap();
    let (log, literal) = log_path(&log_dir);
    let plugins = format!(
        "import fs from \"node:fs\";\n{}\nexport default [debounced];\n",
        DEBOUNCED.replace("__LOG__", &literal)
    );
    let mut files: Vec<(String, String)> = vec![
        (
            "package.json".into(),
            r#"{"name":"burst","version":"1.0.0"}"#.into(),
        ),
        ("index.html".into(), INDEX_HTML.into()),
        ("src/main.js".into(), "window.__ok = 1;\n".into()),
        ("oj.plugins.mjs".into(), plugins),
    ];
    files.extend(message_files());
    let borrowed: Vec<(&str, &str)> = files
        .iter()
        .map(|(p, c)| (p.as_str(), c.as_str()))
        .collect();
    let app = App::new(&borrowed);
    let server = app.serve().await;
    assert!(server.text("/src/main.js").await.contains("__ok"));

    let edits: Vec<(String, String)> = message_files()
        .into_iter()
        .map(|(p, c)| (p, c.replace("\"v", "\"edited-v")))
        .collect();
    let borrowed: Vec<(&str, &str)> = edits
        .iter()
        .map(|(p, c)| (p.as_str(), c.as_str()))
        .collect();
    app.write(&borrowed);

    assert_one_rebuild_each(&settle(&log, FILES).await);
}

/// The runner-backed environments (a plugin's own `dev.createEnvironment`, as
/// Cloudflare's and Start's server environments are) get their changes from
/// `/__oj_invalidate`. A burst arrives as one request per watcher batch, all
/// in flight at once, and must reach the hooks concurrently too.
#[tokio::test(flavor = "multi_thread")]
async fn a_burst_reaches_runner_environment_hot_update_hooks_concurrently() {
    require_fixture!("vite");
    let log_dir = tempfile::tempdir().unwrap();
    let (log, literal) = log_path(&log_dir);
    let config = format!(
        r#"import fs from "node:fs";
import {{ DevEnvironment }} from "vite";
{}
const runner = {{
  name: "runner-env",
  // The middleware server, and with it /__oj_invalidate, starts once a
  // plugin registers middleware, as a runner plugin's dispatch does.
  configureServer(server) {{
    server.middlewares.use((req, res, next) => next());
  }},
  config() {{
    return {{
      environments: {{
        worker: {{
          consumer: "server",
          dev: {{ createEnvironment: (name, config) => new DevEnvironment(name, config, {{ hot: true }}) }},
        }},
      }},
    }};
  }},
}};
export default {{ plugins: [runner, debounced] }};
"#,
        DEBOUNCED.replace("__LOG__", &literal)
    );
    let mut files: Vec<(String, String)> = vec![
        (
            "package.json".into(),
            r#"{"name":"burst-runner","private":true,"type":"module"}"#.into(),
        ),
        ("index.html".into(), INDEX_HTML.into()),
        ("src/main.js".into(), "window.__ok = 1;\n".into()),
        ("vite.config.mjs".into(), config),
    ];
    files.extend(message_files());
    let borrowed: Vec<(&str, &str)> = files
        .iter()
        .map(|(p, c)| (p.as_str(), c.as_str()))
        .collect();
    let app = App::new(&borrowed);
    app.link_fixture("vite");
    let server = app.serve().await;
    middleware_ready(&server).await;
    assert!(
        server.built.plugin_serve.runner_environments(),
        "the declared worker environment never came up"
    );
    let port = server.built.plugin_serve.mw_port().unwrap();

    // Half the burst in one batch, the rest one batch per file, all at once.
    let paths: Vec<String> = message_files()
        .into_iter()
        .map(|(p, _)| app.root.join(p).display().to_string())
        .collect();
    let mut sends = vec![paths[..FILES / 2].to_vec()];
    sends.extend(paths[FILES / 2..].iter().map(|p| vec![p.clone()]));
    let sends = sends.into_iter().map(|batch| async move {
        let changes: Vec<(String, &'static str)> =
            batch.into_iter().map(|p| (p, "update")).collect();
        oj_server::notify_plugin_mw_invalidate(port, &changes).await
    });
    futures_util::future::join_all(sends).await;

    assert_one_rebuild_each(&settle(&log, FILES).await);
}

/// `watchChange` runs before HMR for each change, concurrently across a
/// burst's files too: a plugin debouncing there sees the newer call.
#[tokio::test(flavor = "multi_thread")]
async fn a_burst_runs_watch_change_hooks_concurrently() {
    let log_dir = tempfile::tempdir().unwrap();
    let (log, literal) = log_path(&log_dir);
    let plugin = DEBOUNCED
        .replace("__LOG__", &literal)
        .replace("async hotUpdate({ file })", "async watchChange(file)")
        .replace(
            "((this && this.environment && this.environment.name) || \"default\")",
            "(\"watch:\" + this.environment.name)",
        );
    let plugins = format!("import fs from \"node:fs\";\n{plugin}\nexport default [debounced];\n");
    let mut files: Vec<(String, String)> = vec![
        (
            "package.json".into(),
            r#"{"name":"burst-watch","version":"1.0.0"}"#.into(),
        ),
        ("index.html".into(), INDEX_HTML.into()),
        ("src/main.js".into(), "window.__ok = 1;\n".into()),
        ("oj.plugins.mjs".into(), plugins),
    ];
    files.extend(message_files());
    let borrowed: Vec<(&str, &str)> = files
        .iter()
        .map(|(p, c)| (p.as_str(), c.as_str()))
        .collect();
    let app = App::new(&borrowed);
    let server = app.serve().await;
    assert!(server.text("/src/main.js").await.contains("__ok"));

    let edits: Vec<(String, String)> = message_files()
        .into_iter()
        .map(|(p, c)| (p, c.replace("\"v", "\"edited-v")))
        .collect();
    let borrowed: Vec<(&str, &str)> = edits
        .iter()
        .map(|(p, c)| (p.as_str(), c.as_str()))
        .collect();
    app.write(&borrowed);

    let counts = settle(&log, FILES).await;
    assert!(
        counts.len() == 1 && counts.keys().all(|tag| tag.ends_with(":watch:client")),
        "{counts:?}"
    );
    assert_one_rebuild_each(&counts);
}

/// Within one file, Vite's handleHMRUpdate runs the client environment's
/// `hotUpdate` hooks before any other environment's.
#[tokio::test(flavor = "multi_thread")]
async fn a_files_client_hot_update_runs_before_its_ssr_one() {
    let log_dir = tempfile::tempdir().unwrap();
    let (log, literal) = log_path(&log_dir);
    let plugins = format!(
        r#"import fs from "node:fs";
export default [{{
  name: "order",
  async hotUpdate({{ file }}) {{
    if (!file.endsWith("/src/main.js")) return;
    const env = (this && this.environment && this.environment.name) || "default";
    fs.appendFileSync({literal}, env + "\n");
  }},
}}];
"#
    );
    let app = App::new(&[
        ("package.json", r#"{"name":"hot-order","version":"1.0.0"}"#),
        ("index.html", INDEX_HTML),
        ("src/main.js", "window.__ok = 1;\n"),
        ("oj.plugins.mjs", &plugins),
    ]);
    let server = app.serve().await;
    assert!(server.text("/src/main.js").await.contains("__ok"));
    // Brings the ssr plugin host up, as an SSR request does.
    let main = app.root.join("src/main.js");
    server
        .text(&format!("/@ssr-module?id={}", main.display()))
        .await;

    app.write(&[("src/main.js", "window.__ok = 2;\n")]);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let lines = loop {
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        let lines: Vec<String> = text.lines().map(str::to_string).collect();
        if lines.len() >= 2 {
            break lines;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "hotUpdate never ran in both environments: {lines:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(lines, ["client", "ssr"], "client first, as Vite");
}
