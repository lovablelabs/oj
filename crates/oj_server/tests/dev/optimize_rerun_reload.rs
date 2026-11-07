// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! When a dep rerun reloads the page, in a browser, by Vite's rule
//! (`runOptimizer` in Vite's optimizer): a rerun commits without a reload
//! unless a dep the page already loaded changed its `fileHash` (its file and
//! its entry chunk's `JSON.stringify(chunk.imports)`). A reload is one
//! `full-reload`, and the reloaded page loads each dep file under one URL.
//! (Vite's other trigger, an interop guess the bundle contradicts, needs an
//! ESM dep that rolldown collapses to a lone default; none of the shapes
//! tried here does, under Vite 8 or oj.)
//!
//! The page counts its own loads in `sessionStorage`, so a reload shows as a
//! second load. The late dep comes in through `src/late.js`, which the page
//! imports by a computed specifier on demand: the startup scan and the eager
//! crawl never see it, so the first request for it discovers the dep.

use std::collections::HashMap;
use std::time::Duration;

use crate::browser::Page;
use crate::App;

const WAIT: Duration = Duration::from_secs(30);
/// How long a page must stay unreloaded after the rerun committed: the
/// reload is sent at commit, so it would land well within this.
const SETTLE: Duration = Duration::from_millis(1500);

const INDEX: &str = r#"<!doctype html><html><head><title>t</title></head><body><p id="a"></p><p id="late"></p><script type="module" src="/src/main.js"></script></body></html>"#;

const MAIN: &str = r#"import { a } from "served-a";
sessionStorage.loads = String(Number(sessionStorage.loads || 0) + 1);
document.getElementById("a").textContent = a();
// Not a literal: neither the scan nor the eager crawl follows it.
const late = ["/src", "late.js"].join("/");
window.loadLate = () =>
  import(/* @vite-ignore */ late).then((m) => {
    document.getElementById("late").textContent = m.late;
  });
"#;

/// The page and its one served dep, `served-a`, whose `shared.js` a late dep
/// can share; `late_js` is `src/late.js`, `extra` the late packages.
fn app(late_js: &str, extra: &[(&str, &str)]) -> App {
    let mut files =
        vec![
        ("src/late.js", late_js),
        ("package.json", r#"{"name":"rerun-reload","version":"1.0.0"}"#),
        ("index.html", INDEX),
        ("src/main.js", MAIN),
        (
            "node_modules/served-a/package.json",
            r#"{"name":"served-a","version":"1.0.0","type":"module","main":"index.js"}"#,
        ),
        (
            "node_modules/served-a/index.js",
            "import { store } from \"./shared.js\";\nexport { store };\nexport const a = () => \"a-\" + store.id;\n",
        ),
        (
            "node_modules/served-a/shared.js",
            "export const store = { id: \"s\" };\n",
        ),
    ];
    files.extend_from_slice(extra);
    App::new(&files)
}

/// An ESM package `name` whose `index.js` is `code`.
fn esm(name: &str, code: &str) -> [(String, String); 2] {
    [
        (
            format!("node_modules/{name}/package.json"),
            format!(r#"{{"name":"{name}","version":"1.0.0","type":"module","main":"index.js"}}"#),
        ),
        (format!("node_modules/{name}/index.js"), code.to_string()),
    ]
}

fn owned(files: &[(String, String)]) -> Vec<(&str, &str)> {
    files
        .iter()
        .map(|(p, c)| (p.as_str(), c.as_str()))
        .collect()
}

/// The page, loaded once and showing `served-a`.
/// The listener rides along: dropping it closes the port.
async fn open(app: &App) -> (crate::Server, crate::listen::Listening, Page) {
    let server = app.serve().await;
    let listening = server.listen().await;
    let page = Page::open(&format!("http://127.0.0.1:{}/", listening.addr.port())).await;
    page.wait_for(
        r#"document.getElementById("a")?.textContent === "a-s" && sessionStorage.loads === "1""#,
        WAIT,
    )
    .await;
    (server, listening, page)
}

/// Has the page import `src/late.js`: the discovery that queues the rerun.
async fn load_late(page: &Page) {
    let _: bool = page.eval("void window.loadLate(); true").await;
}

/// Every `/@oj-deps/` file the current document loaded, by path, with the
/// distinct `?v=` queries it loaded under.
async fn dep_versions(page: &Page) -> HashMap<String, Vec<String>> {
    let urls: Vec<String> = page
        .eval(
            r#"performance.getEntriesByType("resource").map((e) => e.name).filter((n) => n.includes("/@oj-deps/"))"#,
        )
        .await;
    let mut versions: HashMap<String, Vec<String>> = HashMap::new();
    for url in &urls {
        let (path, query) = url.split_once('?').unwrap_or((url, ""));
        let v = versions.entry(path.to_string()).or_default();
        if !v.iter().any(|q| q == query) {
            v.push(query.to_string());
        }
    }
    versions
}

/// The late module rendered on the page that loaded once, and stays so: no
/// reload followed the commit.
async fn assert_no_reload(page: &Page, late: &str) {
    let rendered = format!(r#"document.getElementById("late")?.textContent === "{late}""#);
    // Settles on either outcome: a reload wipes the page before it renders.
    let settled = page
        .poll(
            &format!(r#"{rendered} || sessionStorage.loads !== "1""#),
            WAIT,
        )
        .await;
    tokio::time::sleep(SETTLE).await;
    let loads: String = page.eval("sessionStorage.loads").await;
    assert_eq!(loads, "1", "the rerun reloaded the page");
    assert!(
        settled && page.eval::<bool>(&rendered).await,
        "the late module never rendered: {:?}",
        page.errors()
    );
    let late_now: String = page
        .eval(r#"document.getElementById("late").textContent"#)
        .await;
    assert_eq!(late_now, late, "the page kept its state");
}

/// The rerun reloaded the page once, and the reloaded page loads each dep
/// file under one URL and runs the late module on the same deps.
async fn assert_one_reload(page: &Page, late: &str) {
    let reloaded = page
        .poll(
            r#"sessionStorage.loads === "2" && document.getElementById("a")?.textContent === "a-s""#,
            WAIT,
        )
        .await;
    assert!(
        reloaded,
        "the rerun never reloaded the page: {:?}",
        page.errors()
    );
    let _: bool = page.eval("void window.loadLate(); true").await;
    page.wait_for(
        &format!(r#"document.getElementById("late")?.textContent === "{late}""#),
        WAIT,
    )
    .await;
    tokio::time::sleep(SETTLE).await;
    let loads: String = page.eval("sessionStorage.loads").await;
    assert_eq!(loads, "2", "one reload, not one per wave");
    let twice: Vec<_> = dep_versions(page)
        .await
        .into_iter()
        .filter(|(_, v)| v.len() > 1)
        .collect();
    assert!(twice.is_empty(), "dep files under two versions: {twice:#?}");
}

// Vite: a rerun whose new dep shares nothing with the served ones leaves
// every served entry's fileHash alone, so it commits without a reload, and
// the served deps keep their URLs (`browserHash` carried over).
#[tokio::test(flavor = "multi_thread")]
async fn an_unrelated_late_dep_commits_without_a_reload() {
    if crate::browser::skip() {
        return;
    }
    let solo = esm("late-solo", "export const late = \"solo\";\n");
    let app = app("export { late } from \"late-solo\";\n", &owned(&solo));
    let (server, _listening, page) = open(&app).await;
    let before = server.text("/src/main.js").await;
    load_late(&page).await;
    assert_no_reload(&page, "solo").await;
    assert_eq!(
        server.text("/src/main.js").await,
        before,
        "served-a keeps its URL"
    );
    assert_eq!(page.errors(), Vec::<String>::new(), "page errors");
    page.close().await;
}

// Vite: a new dep sharing a served dep's module (an object, which no
// bundler can inline) splits it into a chunk, which moves the served entry's
// chunk imports: its fileHash changes, so the commit reloads, and the
// reloaded page shares one instance of the module.
#[tokio::test(flavor = "multi_thread")]
async fn a_late_dep_moving_a_served_deps_chunk_reloads_once() {
    if crate::browser::skip() {
        return;
    }
    let shared = esm(
        "late-shared",
        "import { store } from \"served-a/shared.js\";\nimport { store as viaA } from \"served-a\";\nexport const late = (store === viaA ? \"one-\" : \"two-\") + store.id;\n",
    );
    let app = app("export { late } from \"late-shared\";\n", &owned(&shared));
    let (_server, _listening, page) = open(&app).await;
    load_late(&page).await;
    assert_one_reload(&page, "one-s").await;
    page.close().await;
}

// Vite's fileHash is the entry's chunk imports, not its code: a rerun after a
// served dep's code changed (its chunks did not) commits without a reload, as
// `optimized dependencies unchanged`. The page keeps the code it loaded.
#[tokio::test(flavor = "multi_thread")]
async fn a_served_deps_code_change_alone_does_not_reload() {
    if crate::browser::skip() {
        return;
    }
    let solo = esm("late-solo", "export const late = \"solo\";\n");
    let app = app("export { late } from \"late-solo\";\n", &owned(&solo));
    let (_server, _listening, page) = open(&app).await;
    app.write(&[(
        "node_modules/served-a/index.js",
        "import { store } from \"./shared.js\";\nexport { store };\nexport const a = () => \"a2-\" + store.id;\n",
    )]);
    load_late(&page).await;
    assert_no_reload(&page, "solo").await;
    let a: String = page
        .eval(r#"document.getElementById("a").textContent"#)
        .await;
    assert_eq!(a, "a-s", "the page kept the served-a it loaded");
    page.close().await;
}
