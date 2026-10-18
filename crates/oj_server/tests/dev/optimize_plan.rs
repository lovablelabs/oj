// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Vite's cold-boot dep optimization, end to end through the plugin host's
//! scan (scanProcessing itself is covered in optimize.rs's unit tests):
//! - the plan they compile against must match what the bundle commits for
//!   every common dep shape (react's named facade, the jsx runtime, plain
//!   CJS, ESM): a miss bumps the deps version and reloads the page;
//! - a dep the crawl finds that the scan missed joins the first pre-bundle
//!   (Vite's holdUntilCrawlEnd) instead of costing a rerun.

use std::path::Path;
use std::time::{Duration, Instant};

use regex::Regex;

use crate::App;

const INDEX: &str = r#"<!doctype html><html><head></head><body><script type="module" src="/main.js"></script></body></html>"#;

fn dep_urls(code: &str) -> Vec<String> {
    Regex::new(r#"(?:from|import)\s*"(/@oj-deps/[^"]+)""#)
        .unwrap()
        .captures_iter(code)
        .map(|c| c[1].to_string())
        .collect()
}

/// The first deps manifest the commit writes.
async fn first_manifest(root: &Path) -> serde_json::Value {
    let path = oj_cache::cache_root(root).join("deps/manifest.json");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(v) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
        {
            return v;
        }
        assert!(Instant::now() < deadline, "no deps manifest was written");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn plain_dep(name: &str, cjs: bool) -> [(String, String); 2] {
    let (kind, body) = if cjs {
        ("commonjs", "exports.hi = () => 1;\n")
    } else {
        ("module", "export const hi = () => 1;\n")
    };
    [
        (
            format!("node_modules/{name}/package.json"),
            format!(r#"{{"name":"{name}","version":"1.0.0","type":"{kind}","main":"index.js"}}"#),
        ),
        (format!("node_modules/{name}/index.js"), body.to_string()),
    ]
}

fn app_with(files: &[(String, String)]) -> App {
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(p, b)| (p.as_str(), b.as_str()))
        .collect();
    App::new(&refs)
}

#[tokio::test(flavor = "multi_thread")]
async fn cold_boot_plan_matches_the_committed_pre_bundle() {
    crate::require_fixture!("vite", "react", "react-dom");
    let mut files = vec![
        (
            "package.json".to_string(),
            r#"{"name":"plan-app","private":true,"type":"module"}"#.to_string(),
        ),
        ("vite.config.js".into(), "export default {};\n".into()),
        ("index.html".into(), INDEX.into()),
        (
            "main.js".into(),
            "import { useState } from \"react\";\n\
             import { createRoot } from \"react-dom/client\";\n\
             import { hi as a } from \"plain-cjs\";\n\
             import { hi as b } from \"plain-esm\";\n\
             window.__R = [useState, createRoot, a(), b()];\n"
                .into(),
        ),
    ];
    files.extend(plain_dep("plain-cjs", true));
    files.extend(plain_dep("plain-esm", false));
    let app = app_with(&files);
    for pkg in ["vite", "react", "react-dom"] {
        app.link_fixture(pkg);
    }
    let server = app.serve().await;

    let first = dep_urls(&server.text("/main.js").await);
    assert_eq!(
        first.len(),
        4,
        "every dep served from the pre-bundle: {first:?}"
    );
    // Each dep request waits for the commit.
    for url in &first {
        server.text(url).await;
    }
    let manifest = first_manifest(&app.root).await;
    let hash = manifest["hash"].as_str().unwrap();
    assert_eq!(
        manifest["browserHash"].as_str().unwrap(),
        &hash[..8],
        "the commit kept the planned version (a contradicted plan bumps it): {manifest:#}"
    );
    assert_eq!(
        dep_urls(&server.text("/main.js").await),
        first,
        "the committed URLs are the ones served before the commit"
    );
}

/// An app whose plugin transform adds an import (`late-esm`) the scan cannot
/// see, served without waiting for the file watcher (by then any rerun has
/// long committed too, and the first commit is unobservable). Returns the
/// app, the server, main.js's dep URLs and the manifest the first commit
/// wrote, read as soon as the planned dep's held request returns.
async fn missed_dep_app(
    optimize_deps: &str,
) -> (App, crate::Server, Vec<String>, serde_json::Value) {
    let mut files = vec![
        (
            "package.json".to_string(),
            r#"{"name":"hold-app","private":true,"type":"module"}"#.to_string(),
        ),
        (
            "vite.config.js".into(),
            format!(
                r#"export default {{
  optimizeDeps: {optimize_deps},
  plugins: [{{
    name: "inject-import",
    transform(code, id) {{
      if (id.endsWith("/main.js")) return 'import {{ hi as late }} from "late-esm";\nwindow.__L = late();\n' + code;
    }},
  }}],
}};
"#
            ),
        ),
        ("index.html".into(), INDEX.into()),
        (
            "main.js".into(),
            "import { hi } from \"plain-esm\";\nwindow.__R = hi();\n".into(),
        ),
    ];
    files.extend(plain_dep("plain-esm", false));
    files.extend(plain_dep("late-esm", false));
    let app = app_with(&files);
    app.link_fixture("vite");
    let built = app.dev().build_app().await.expect("dev server builds");
    let server = crate::Server {
        built,
        root: app.root.clone(),
    };
    let urls = dep_urls(&server.text("/main.js").await);
    assert_eq!(urls.len(), 2, "both deps optimized: {urls:?}");
    // Only the planned dep: its request returns with the first commit.
    let planned = urls.iter().find(|u| u.contains("plain-esm")).unwrap();
    server.text(planned).await;
    let manifest = first_manifest(&app.root).await;
    (app, server, urls, manifest)
}

// Vite's holdUntilCrawlEnd (its default): the crawl finds the dep the scan
// missed while the first pre-bundle runs, so the commit waits for the crawl
// and bundles it too, instead of committing and paying a rerun.
#[tokio::test(flavor = "multi_thread")]
async fn a_dep_the_scan_missed_joins_the_first_pre_bundle() {
    crate::require_fixture!("vite");
    let (_app, server, first, manifest) = missed_dep_app("{}").await;
    assert_eq!(
        manifest["discovered"],
        serde_json::json!(["late-esm"]),
        "the missed dep joined the first commit: {manifest:#}"
    );
    assert!(
        manifest["metadata"]["late-esm"]["fileHash"]
            .as_str()
            .is_some_and(|h| !h.is_empty()),
        "and was bundled by it: {manifest:#}"
    );
    let hash = manifest["hash"].as_str().unwrap();
    assert_eq!(
        manifest["browserHash"].as_str().unwrap(),
        &hash[..8],
        "no reload: {manifest:#}"
    );
    assert_eq!(
        dep_urls(&server.text("/main.js").await),
        first,
        "every URL keeps the version it was served under"
    );
}

// `holdUntilCrawlEnd: false`: Vite releases the first pre-bundle as soon as
// it is done; the missed dep is left to the rerun.
#[tokio::test(flavor = "multi_thread")]
async fn without_the_hold_the_first_commit_leaves_the_missed_dep_to_a_rerun() {
    crate::require_fixture!("vite");
    let (_app, server, _, manifest) = missed_dep_app("{ holdUntilCrawlEnd: false }").await;
    assert_eq!(
        manifest["discovered"],
        serde_json::json!([]),
        "the first commit did not wait: {manifest:#}"
    );
    assert!(
        manifest["metadata"].get("late-esm").is_none(),
        "{manifest:#}"
    );
    // The rerun commits it.
    let late = dep_urls(&server.text("/main.js").await)
        .into_iter()
        .find(|u| u.contains("late-esm"))
        .unwrap();
    assert_eq!(server.get(&late).await.status, 200);
}
