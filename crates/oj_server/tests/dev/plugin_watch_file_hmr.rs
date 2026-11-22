// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A file a plugin watches from a module's transform (`this.addWatchFile`)
//! is one of that module's imports in Vite's graph (`_addedImports`:
//! importAnalysis for a JS module, css-analysis's file-only entries for a
//! sheet), so an edit to it walks up from that module like any dep's edit: a
//! sheet swaps, an accepting module re-imports, and only a chain with no
//! boundary reloads. Tailwind watches every source it scans this way. A file
//! watched from outside a transform (`buildStart`) is in no module's imports
//! and its edit sends nothing. Being watched never reloads the page.

use axum::body::Body;
use axum::http::Request;

use crate::listen::{close, frame_matching};
use crate::plugin_middleware::middleware_ready;
use crate::{App, Server};

const INDEX: &str = r#"<!doctype html><html><head><link rel="stylesheet" href="/src/style.css"></head><body><script type="module" src="/src/main.js"></script></body></html>"#;

fn config(root: &std::path::Path) -> String {
    format!(
        r#"import fs from "node:fs";
import path from "node:path";
const root = {root};
const at = (rel) => path.join(root, rel);
const read = (rel) => fs.readFileSync(at(rel), "utf8").trim();
export default {{
  plugins: [{{
    name: "watcher",
    buildStart() {{ this.addWatchFile(at("boot.txt")); }},
    // The harness waits on the middleware port for the plugins to be up.
    configureServer(server) {{ server.middlewares.use((_req, _res, next) => next()); }},
    transform(code, id) {{
      // A linked sheet's id carries `?direct`.
      id = id.split("?")[0];
      if (id.endsWith("/src/style.css")) {{
        this.addWatchFile(at("src/tokens.txt"));
        this.addWatchFile(at("src/widget.js"));
        return code + `\n.token::after {{ content: "${{read("src/tokens.txt")}}" }}\n`;
      }}
      if (id.endsWith("/src/accepting.js")) {{
        this.addWatchFile(at("src/note.txt"));
        return code.replace("__NOTE__", read("src/note.txt"));
      }}
      if (id.endsWith("/src/plain.js")) {{
        this.addWatchFile(at("src/plain.txt"));
        return code.replace("__PLAIN__", read("src/plain.txt"));
      }}
    }},
  }}],
}};
"#,
        root = serde_json::to_string(&root.display().to_string()).unwrap()
    )
}

async fn boot() -> (App, Server) {
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"watch-file-hmr","version":"1.0.0"}"#,
        ),
        ("boot.txt", "boot-v1\n"),
        ("src/style.css", ".a { color: red }\n"),
        ("src/tokens.txt", "token-v1\n"),
        ("src/note.txt", "note-v1\n"),
        ("src/plain.txt", "plain-v1\n"),
        (
            "src/main.js",
            "import \"./accepting.js\";\nimport \"./plain.js\";\nimport \"./widget.js\";\n",
        ),
        (
            "src/accepting.js",
            "export const note = \"__NOTE__\";\nif (import.meta.hot) import.meta.hot.accept();\n",
        ),
        ("src/plain.js", "export const plain = \"__PLAIN__\";\n"),
        (
            "src/widget.js",
            "export const widget = 1;\nif (import.meta.hot) import.meta.hot.accept();\n",
        ),
        ("index.html", INDEX),
    ]);
    app.write(&[("vite.config.mjs", &config(&app.root))]);
    let server = app.serve().await;
    middleware_ready(&server).await;
    // In the graph, with what each transform watched.
    for module in [
        "/src/main.js",
        "/src/accepting.js",
        "/src/plain.js",
        "/src/widget.js",
    ] {
        server.text(module).await;
    }
    let sheet = linked_sheet(&server).await;
    assert!(sheet.contains("token-v1"), "{sheet}");
    (app, server)
}

async fn linked_sheet(server: &Server) -> String {
    let res = server
        .request(
            Request::get("/src/style.css")
                .header("sec-fetch-dest", "style")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(res.status, 200, "{}", res.body);
    res.body
}

/// What the page is told to do: the next `update` or `full-reload` frame.
async fn next_decision(socket: &mut crate::listen::Ws) -> serde_json::Value {
    frame_matching(socket, "an update or a full-reload", |f| {
        f["type"] == "update" || f["type"] == "full-reload"
    })
    .await
}

fn updates(frame: &serde_json::Value) -> Vec<(String, String)> {
    frame["updates"]
        .as_array()
        .map(|all| {
            all.iter()
                .map(|u| {
                    (
                        u["type"].as_str().unwrap_or_default().to_string(),
                        u["path"].as_str().unwrap_or_default().to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn entry(kind: &str, path: &str) -> (String, String) {
    (kind.to_string(), path.to_string())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_a_sheets_transform_watched_swaps_the_sheet() {
    let (app, server) = boot().await;
    let listening = server.listen().await;
    let (mut socket, _) = server.ws(listening.addr, "/__ws", None).await;

    app.write(&[("src/tokens.txt", "token-v2\n")]);
    let frame = next_decision(&mut socket).await;
    assert_eq!(frame["type"], "update", "{frame}");
    assert_eq!(
        updates(&frame),
        [entry("css-update", "/src/style.css")],
        "{frame}"
    );
    let sheet = linked_sheet(&server).await;
    assert!(sheet.contains("token-v2"), "{sheet}");
    close(socket).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_an_accepting_modules_transform_watched_updates_the_module() {
    let (app, server) = boot().await;
    let listening = server.listen().await;
    let (mut socket, _) = server.ws(listening.addr, "/__ws", None).await;

    app.write(&[("src/note.txt", "note-v2\n")]);
    let frame = next_decision(&mut socket).await;
    assert_eq!(frame["type"], "update", "{frame}");
    assert_eq!(
        updates(&frame),
        [entry("js-update", "/src/accepting.js")],
        "{frame}"
    );
    let stamp = frame["updates"][0]["timestamp"].as_u64().unwrap();
    let module = server.text(&format!("/src/accepting.js?t={stamp}")).await;
    assert!(module.contains("note-v2"), "{module}");
    close(socket).await;
}

/// Vite's propagation from the file-only entry reaches the page's entry with
/// nothing accepting on the way: a dead end, which reloads.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_watched_by_a_module_with_no_boundary_above_it_reloads() {
    let (app, server) = boot().await;
    let listening = server.listen().await;
    let (mut socket, _) = server.ws(listening.addr, "/__ws", None).await;

    app.write(&[("src/plain.txt", "plain-v2\n")]);
    let frame = next_decision(&mut socket).await;
    assert_eq!(frame["type"], "full-reload", "{frame}");
    close(socket).await;
}

/// The watched file is a module too: Vite's `getModulesByFile` holds both
/// its module node and its file-only entry, and each walks up on its own.
#[tokio::test(flavor = "multi_thread")]
async fn a_watched_file_that_is_a_module_updates_itself_and_its_watcher() {
    let (app, server) = boot().await;
    let listening = server.listen().await;
    let (mut socket, _) = server.ws(listening.addr, "/__ws", None).await;

    app.write(&[(
        "src/widget.js",
        "export const widget = 2;\nif (import.meta.hot) import.meta.hot.accept();\n",
    )]);
    let frame = next_decision(&mut socket).await;
    assert_eq!(frame["type"], "update", "{frame}");
    let mut got = updates(&frame);
    got.sort();
    assert_eq!(
        got,
        [
            entry("css-update", "/src/style.css"),
            entry("js-update", "/src/widget.js"),
        ],
        "{frame}"
    );
    close(socket).await;
}

/// `buildStart`'s watch file is no module's import: Vite finds no module for
/// it (`[no modules matched]`) and sends nothing. The edit after it shows
/// nothing was sent ahead of it.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_watched_outside_a_transform_sends_nothing() {
    let (app, server) = boot().await;
    let listening = server.listen().await;
    let (mut socket, _) = server.ws(listening.addr, "/__ws", None).await;

    app.write(&[("boot.txt", "boot-v2\n")]);
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    app.write(&[("src/note.txt", "note-v2\n")]);
    let frame = next_decision(&mut socket).await;
    assert_eq!(frame["type"], "update", "{frame}");
    assert_eq!(
        updates(&frame),
        [entry("js-update", "/src/accepting.js")],
        "{frame}"
    );
    close(socket).await;
}
