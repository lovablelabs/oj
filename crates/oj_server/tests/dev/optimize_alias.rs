// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A `resolve.alias` that points a bare dep at its own node_modules path (the
//! `react: path.resolve("node_modules/react")` shape) must not split the dep
//! in two. Vite's scan resolves a bare import through the plugin container,
//! alias included, and records it under its bare name when the result lands
//! in node_modules (optimizer/scan.ts), so the aliased dep is a pre-bundle
//! entry like any other and a dep importing it shares its chunk. Skipped by
//! the scan, it was inlined into every dep that imports it, then optimized a
//! second time when the app's own import discovered it: two Reacts, and a
//! null `useEffect` on a fresh load.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::App;

const INDEX: &str = r#"<!doctype html><html><head></head><body><script type="module" src="/main.js"></script></body></html>"#;

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

#[tokio::test(flavor = "multi_thread")]
async fn an_alias_to_a_dep_in_node_modules_keeps_one_copy_of_it() {
    crate::require_fixture!("vite");
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"alias-dedupe","private":true,"type":"module"}"#,
        ),
        (
            "vite.config.js",
            "import path from \"node:path\";\n\
             const dir = path.dirname(new URL(import.meta.url).pathname);\n\
             export default { resolve: { alias: { \"core-lib\": path.resolve(dir, \"node_modules/core-lib\") } } };\n",
        ),
        ("index.html", INDEX),
        (
            "main.js",
            "import { core } from \"core-lib\";\n\
             import { userCore } from \"user-lib\";\n\
             window.__SAME = core === userCore;\n",
        ),
        (
            "node_modules/core-lib/package.json",
            r#"{"name":"core-lib","version":"1.0.0","type":"module","main":"index.js"}"#,
        ),
        (
            "node_modules/core-lib/index.js",
            "export const core = { marker: \"core-lib-singleton\" };\n",
        ),
        (
            "node_modules/user-lib/package.json",
            r#"{"name":"user-lib","version":"1.0.0","type":"module","main":"index.js"}"#,
        ),
        (
            "node_modules/user-lib/index.js",
            "import { core } from \"core-lib\";\nexport const userCore = core;\n",
        ),
    ]);
    app.link_fixture("vite");
    let server = app.serve().await;
    server.text("/main.js").await;

    let manifest = first_manifest(&app.root).await;
    let meta = &manifest["metadata"];
    assert!(
        meta.get("core-lib").is_some() && meta.get("user-lib").is_some(),
        "the aliased dep is a pre-bundle entry from the first scan: {manifest:#}"
    );
    let deps = oj_cache::cache_root(&app.root).join("deps");
    let user_file = meta["user-lib"]["file"].as_str().unwrap();
    let user = std::fs::read_to_string(deps.join(user_file)).unwrap();
    assert!(
        !user.contains("core-lib-singleton"),
        "user-lib inlined its own copy of the aliased dep:\n{user}"
    );
    let copies = std::fs::read_dir(&deps)
        .unwrap()
        .flatten()
        .filter(|e| {
            std::fs::read_to_string(e.path()).is_ok_and(|code| code.contains("core-lib-singleton"))
        })
        .count();
    assert_eq!(
        copies, 1,
        "one copy of the aliased dep across the pre-bundle"
    );
}

/// An alias into the app's own source (`@/x`) is crawled, as Vite's scan
/// crawls any bare import that resolves outside node_modules: a dep imported
/// only from such a file joins the first pre-bundle.
#[tokio::test(flavor = "multi_thread")]
async fn an_alias_into_source_is_crawled_for_its_deps() {
    crate::require_fixture!("vite");
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"alias-src","private":true,"type":"module"}"#,
        ),
        (
            "vite.config.js",
            "import path from \"node:path\";\n\
             const dir = path.dirname(new URL(import.meta.url).pathname);\n\
             export default { resolve: { alias: { \"@\": path.resolve(dir, \"src\") } } };\n",
        ),
        ("index.html", INDEX),
        (
            "main.js",
            "import { hi } from \"@/util.js\";\nwindow.__R = hi();\n",
        ),
        (
            "src/util.js",
            "import { hi as inner } from \"deep-lib\";\nexport const hi = () => inner();\n",
        ),
        (
            "node_modules/deep-lib/package.json",
            r#"{"name":"deep-lib","version":"1.0.0","type":"module","main":"index.js"}"#,
        ),
        (
            "node_modules/deep-lib/index.js",
            "export const hi = () => 1;\n",
        ),
    ]);
    app.link_fixture("vite");
    let server = app.serve().await;
    server.text("/main.js").await;

    let manifest = first_manifest(&app.root).await;
    assert!(
        manifest["metadata"].get("deep-lib").is_some(),
        "the dep behind the source alias joined the first pre-bundle: {manifest:#}"
    );
    assert!(
        manifest["metadata"].get("@/util.js").is_none(),
        "app source is never a dep: {manifest:#}"
    );
}
