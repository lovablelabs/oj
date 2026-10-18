// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A cold boot serves source modules against the plan the plugin host's scan
//! guesses for the dep pre-bundle, before the bundle commits. The guess must
//! match what the bundle commits for every common dep shape (react's named
//! facade, the jsx runtime, plain CJS, ESM): a miss bumps the deps version
//! and reloads the page, which no other test would notice.

use std::time::{Duration, Instant};

use regex::Regex;

use crate::App;

#[tokio::test(flavor = "multi_thread")]
async fn cold_boot_plan_matches_the_committed_pre_bundle() {
    crate::require_fixture!("vite", "react", "react-dom");
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"plan-app","private":true,"type":"module"}"#,
        ),
        ("vite.config.js", "export default {};\n"),
        (
            "index.html",
            r#"<!doctype html><html><head></head><body><script type="module" src="/main.js"></script></body></html>"#,
        ),
        (
            "node_modules/plain-cjs/package.json",
            r#"{"name":"plain-cjs","version":"1.0.0","main":"index.js"}"#,
        ),
        (
            "node_modules/plain-cjs/index.js",
            "exports.hi = () => \"cjs\";\n",
        ),
        (
            "node_modules/plain-esm/package.json",
            r#"{"name":"plain-esm","version":"1.0.0","type":"module","main":"index.js"}"#,
        ),
        (
            "node_modules/plain-esm/index.js",
            "export const hi = () => \"esm\";\n",
        ),
        (
            "main.js",
            "import { useState } from \"react\";\n\
             import { createRoot } from \"react-dom/client\";\n\
             import { hi as a } from \"plain-cjs\";\n\
             import { hi as b } from \"plain-esm\";\n\
             window.__R = [useState, createRoot, a(), b()];\n",
        ),
    ]);
    for pkg in ["vite", "react", "react-dom"] {
        app.link_fixture(pkg);
    }
    let server = app.serve().await;

    let imports = Regex::new(r#"(?:from|import)\s*"(/@oj-deps/[^"]+)""#).unwrap();
    let dep_urls = |code: &str| -> Vec<String> {
        imports
            .captures_iter(code)
            .map(|c| c[1].to_string())
            .collect()
    };
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

    let manifest = oj_cache::cache_root(&app.root).join("deps/manifest.json");
    let deadline = Instant::now() + Duration::from_secs(30);
    let manifest: serde_json::Value = loop {
        if let Some(v) = std::fs::read_to_string(&manifest)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
        {
            break v;
        }
        assert!(Instant::now() < deadline, "no deps manifest was written");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
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
