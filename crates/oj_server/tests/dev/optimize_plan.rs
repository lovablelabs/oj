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

fn url_version(url: &str) -> &str {
    url.rsplit_once("?v=").map(|(_, v)| v).unwrap_or("")
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
    assert_eq!(
        manifest["browserHash"].as_str().unwrap(),
        url_version(&first[0]),
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
    let planned = first.iter().find(|u| u.contains("plain-esm")).unwrap();
    assert_eq!(
        manifest["browserHash"].as_str().unwrap(),
        url_version(planned),
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

fn plain_esm_app() -> App {
    let mut files = vec![
        (
            "package.json".to_string(),
            r#"{"name":"session-app","private":true,"type":"module"}"#.to_string(),
        ),
        ("vite.config.js".into(), "export default {};\n".into()),
        ("index.html".into(), INDEX.into()),
        (
            "main.js".into(),
            "import { hi } from \"plain-esm\";\nwindow.__R = hi();\n".into(),
        ),
    ];
    files.extend(plain_dep("plain-esm", false));
    let app = app_with(&files);
    app.link_fixture("vite");
    app
}

// Vite's optimizedDeps load: a versioned dep file gone from disk (a rerun or
// another session replaced the bundle) is the outdated 504 a reload gets
// past; only an unversioned miss is a 404.
#[tokio::test(flavor = "multi_thread")]
async fn a_vanished_versioned_dep_file_is_outdated_not_missing() {
    crate::require_fixture!("vite");
    let app = plain_esm_app();
    let server = app.serve().await;
    let url = dep_urls(&server.text("/main.js").await)
        .into_iter()
        .find(|u| u.contains("plain-esm"))
        .unwrap();
    server.text(&url).await;
    let file = url
        .trim_start_matches("/@oj-deps/")
        .split('?')
        .next()
        .unwrap();
    std::fs::remove_file(oj_cache::cache_root(&app.root).join("deps").join(file)).unwrap();
    assert_eq!(server.get(&url).await.status, 504, "versioned: {url}");
    assert_eq!(
        server.get(&format!("/@oj-deps/{file}")).await.status,
        404,
        "unversioned"
    );
}

// Vite's sessionTimestamp: dep URLs are cached immutable, so a cold session
// (a restart after the deps cache was cleared) must not serve its bundle
// under a URL an earlier session already handed out.
#[tokio::test(flavor = "multi_thread")]
async fn a_cold_session_never_reuses_an_earlier_sessions_dep_urls() {
    crate::require_fixture!("vite");
    let app = plain_esm_app();
    let first = {
        let server = app.serve().await;
        let urls = dep_urls(&server.text("/main.js").await);
        for url in &urls {
            server.text(url).await;
        }
        urls
    };
    std::fs::remove_dir_all(oj_cache::cache_root(&app.root).join("deps")).unwrap();
    let server = app.serve().await;
    let second = dep_urls(&server.text("/main.js").await);
    assert_eq!(first.len(), second.len(), "{first:?} vs {second:?}");
    for (a, b) in first.iter().zip(&second) {
        assert_ne!(a, b, "a cold session minted a URL an earlier one served");
        server.text(b).await;
    }
}

// Vite's cached _metadata.json: a warm session reuses the persisted versions,
// so every URL the first session served (and the browser cached immutable)
// still serves the same bundle.
#[tokio::test(flavor = "multi_thread")]
async fn a_warm_session_keeps_every_dep_url() {
    crate::require_fixture!("vite");
    let app = plain_esm_app();
    let first = {
        let server = app.serve().await;
        let urls = dep_urls(&server.text("/main.js").await);
        for url in &urls {
            server.text(url).await;
        }
        first_manifest(&app.root).await;
        urls
    };
    assert!(!first.is_empty());
    let server = app.serve().await;
    let second = dep_urls(&server.text("/main.js").await);
    assert_eq!(first, second, "a warm session moved the dep URLs");
    for url in &second {
        assert_eq!(server.get(url).await.status, 200, "{url}");
    }
}

/// The `/@oj-deps/` modulepreload hrefs oj's dev HTML carries.
fn dep_preloads(html: &str) -> Vec<String> {
    Regex::new(r#"<link rel="modulepreload" href="(/@oj-deps/[^"]+)""#)
        .unwrap()
        .captures_iter(html)
        .map(|c| c[1].to_string())
        .collect()
}

/// `a-esm` and `b-esm` share `shared-esm` (a chunk of the first pre-bundle);
/// `late-esm` is imported only by `extra.js`, which no entry reaches, so the
/// scan misses it and a request for `extra.js` discovers it at serve time.
fn preload_app() -> App {
    let dep = |name: &str, body: &str| {
        [
            (
                format!("node_modules/{name}/package.json"),
                format!(
                    r#"{{"name":"{name}","version":"1.0.0","type":"module","main":"index.js"}}"#
                ),
            ),
            (format!("node_modules/{name}/index.js"), body.to_string()),
        ]
    };
    let mut files = vec![
        (
            "package.json".to_string(),
            r#"{"name":"preload-app","private":true,"type":"module"}"#.to_string(),
        ),
        ("vite.config.js".into(), "export default {};\n".into()),
        ("index.html".into(), INDEX.into()),
        (
            "main.js".into(),
            "import { a } from \"a-esm\";\nimport { b } from \"b-esm\";\nwindow.__R = a() + b();\n"
                .into(),
        ),
        (
            "extra.js".into(),
            "import { late } from \"late-esm\";\nwindow.__L = late();\n".into(),
        ),
    ];
    files.extend(dep("shared-esm", "export const shared = () => \"s\";\n"));
    for name in ["a-esm", "b-esm", "late-esm"] {
        files.extend(dep(
            name,
            "import { shared } from \"shared-esm\";\nexport const a = shared, b = shared, late = shared;\n",
        ));
    }
    let app = app_with(&files);
    app.link_fixture("vite");
    app
}

/// Discovers `late-esm` through `extra.js` and waits for its rerun to commit
/// (the dep route holds a pending dep's request until then).
async fn discover_late(server: &crate::Server) -> String {
    let late = dep_urls(&server.text("/extra.js").await)
        .into_iter()
        .find(|u| u.contains("late-esm"))
        .expect("extra.js imports the late dep through /@oj-deps");
    assert_eq!(server.get(&late).await.status, 200, "{late}");
    late
}

/// Every dep preload in the page is a URL an import names (so preload and
/// import share one cache entry) and serves 200, never the outdated 504.
async fn assert_preloads_match_imports(server: &crate::Server, imports: &[String]) {
    let preloads = dep_preloads(&server.text("/").await);
    for url in imports {
        assert!(
            preloads.contains(url),
            "no preload names the import {url}: {preloads:?}"
        );
    }
    for url in &preloads {
        // A chunk preloads bare, as dep files import it.
        assert!(
            imports.contains(url) || !url.contains("?v="),
            "the preload {url} is no URL an import names: {imports:?}"
        );
        assert_eq!(server.get(url).await.status, 200, "preload {url}");
    }
}

// Vite's resolve gives each optimized entry its own browserHash
// (optimizedDepInfoFromFile), and a rerun that needs no reload keeps the
// committed entries' versions: after a discovery, entries carry different
// versions, so a preload stamped with the session's base version names a URL
// no import uses, and a discovered dep's preload is the outdated 504.
#[tokio::test(flavor = "multi_thread")]
async fn a_discovered_dep_preloads_under_its_own_version() {
    crate::require_fixture!("vite");
    let app = preload_app();
    let server = app.serve().await;
    let first = dep_urls(&server.text("/main.js").await);
    assert_eq!(first.len(), 2, "{first:?}");
    first_manifest(&app.root).await;
    let late = discover_late(&server).await;
    assert_ne!(
        url_version(&late),
        url_version(&first[0]),
        "the discovered dep carries the discovered set's version"
    );
    assert_eq!(
        dep_urls(&server.text("/main.js").await),
        first,
        "no reload: the committed entries keep their versions"
    );
    let mut imports = first;
    imports.push(late);
    assert_preloads_match_imports(&server, &imports).await;
}

// The same on a warm boot: the manifest restores each entry's own version,
// a discovered dep's included, before any request is served.
#[tokio::test(flavor = "multi_thread")]
async fn a_warm_session_preloads_every_entry_under_its_import_url() {
    crate::require_fixture!("vite");
    let app = preload_app();
    {
        let server = app.serve().await;
        server.text("/main.js").await;
        first_manifest(&app.root).await;
        discover_late(&server).await;
    }
    let server = app.serve().await;
    let mut imports = dep_urls(&server.text("/main.js").await);
    imports.extend(dep_urls(&server.text("/extra.js").await));
    assert_eq!(imports.len(), 3, "{imports:?}");
    assert_preloads_match_imports(&server, &imports).await;
}

/// Vite runs `optimizeDeps.rolldownOptions.plugins` in the scan's own
/// rolldown build: a `this.resolve` from any of their hooks reaches the
/// app's plugins as a scan resolve (`scan: true`).
#[tokio::test(flavor = "multi_thread")]
async fn a_scan_plugins_resolve_reaches_the_app_plugins_as_a_scan_resolve() {
    crate::require_fixture!("vite");
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"scan-ctx","private":true,"type":"module"}"#,
        ),
        ("index.html", INDEX),
        ("main.js", "import \"scan-probe\";\n"),
    ]);
    let seen = app.root.with_extension("scan-seen.log");
    app.write(&[(
        "vite.config.mjs",
        &format!(
            r#"import {{ appendFileSync }} from "node:fs";
export default {{
  optimizeDeps: {{
    rolldownOptions: {{
      plugins: [{{
        name: "scan-only",
        async resolveId(id, importer) {{
          if (id === "scan-probe") await this.resolve("from-resolveId", importer, {{ skipSelf: true }});
          return null;
        }},
        async load(id) {{
          if (id.endsWith("/main.js")) await this.resolve("from-load", id);
          return null;
        }},
        async transform(code, id) {{
          if (id.endsWith("/main.js")) await this.resolve("from-transform", id);
          return null;
        }},
      }}],
    }},
  }},
  plugins: [{{
    name: "app-plugin",
    resolveId(id, importer, options) {{
      if (id.startsWith("from-")) appendFileSync({seen:?}, id + ":" + options.scan + "\n");
      return null;
    }},
  }}],
}};
"#
        ),
    )]);
    app.link_fixture("vite");
    let _server = app.serve().await;
    let deadline = Instant::now() + Duration::from_secs(20);
    let seen = loop {
        let text = std::fs::read_to_string(&seen).unwrap_or_default();
        if text.lines().count() >= 3 || Instant::now() > deadline {
            break text;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let mut lines: Vec<&str> = seen.lines().collect();
    lines.sort_unstable();
    lines.dedup();
    assert_eq!(
        lines,
        [
            "from-load:true",
            "from-resolveId:true",
            "from-transform:true"
        ],
        "options.scan the app plugin saw"
    );
}
