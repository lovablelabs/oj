// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Pre-bundle through the REAL engine and the REAL rolldown the fixture's
//! vite brings. An integration test so build.rs's exported-symbols link flag
//! applies: rolldown is a napi addon that resolves napi_* against the running
//! executable. Skips quietly when the fixture is not installed.

use std::path::Path;

use oj_server::optimize::{OptimizeInput, OptimizedDeps};

#[tokio::test(flavor = "multi_thread")]
async fn optimizer_prebundles_through_the_engine_with_real_rolldown() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let vite = repo.join("e2e/fixtures/start-app/node_modules/vite");
    if !vite.exists() {
        eprintln!("skipping: fixture vite not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("node_modules")).unwrap();
    std::fs::write(root.join("package.json"), r#"{"name":"fx"}"#).unwrap();
    std::os::unix::fs::symlink(&vite, root.join("node_modules/vite")).unwrap();
    let dep = root.join("node_modules/plaincjs");
    std::fs::create_dir_all(&dep).unwrap();
    std::fs::write(
        dep.join("package.json"),
        r#"{"name":"plaincjs","version":"1.0.0","main":"index.js"}"#,
    )
    .unwrap();
    std::fs::write(dep.join("index.js"), "exports.a = 1;\nexports.b = 2;\n").unwrap();

    let input = || OptimizeInput {
        include: vec!["plaincjs".into()],
        // As the real callers fill them (empty lists would strip the
        // resolver's mainFields entirely).
        conditions: vec!["browser".into(), "module".into(), "development".into()],
        main_fields: vec!["browser".into(), "module".into(), "main".into()],
        extensions: vec![".mjs".into(), ".js".into(), ".ts".into(), ".json".into()],
        ..Default::default()
    };
    let deps = OptimizedDeps::prepare(root, "0.0.1", input(), None);
    let map = deps.ready().await;
    let meta = map.get("plaincjs").expect("the included dep is bundled");
    assert!(meta.needs_interop, "a plain-CJS bundle needs interop");
    assert!(
        meta.url.starts_with("/@oj-deps/") && meta.url.contains("?v="),
        "versioned optimized-dep URL, got {}",
        meta.url
    );
    let bundle = std::fs::read_to_string(deps.dir().join(&meta.file)).unwrap();
    assert!(bundle.contains("export"), "an ESM pre-bundle was written");

    // The manifest makes the next boot a warm cache (no second optimizer run
    // is observable here, but the warm load must agree with the first).
    let warm = OptimizedDeps::prepare(root, "0.0.1", input(), None)
        .ready()
        .await;
    assert_eq!(
        warm.get("plaincjs").map(|m| &m.file),
        Some(&meta.file),
        "warm manifest load returns the same bundle"
    );

    // Vite's registerMissingImport: a dep the scan never saw (installed or
    // only reachable at serve time) registers, a debounced rerun bundles it,
    // and every optimized URL moves to the bumped browser version.
    let late = root.join("node_modules/latedep");
    std::fs::create_dir_all(&late).unwrap();
    std::fs::write(
        late.join("package.json"),
        r#"{"name":"latedep","version":"1.0.0","main":"index.js"}"#,
    )
    .unwrap();
    std::fs::write(late.join("index.js"), "exports.l = 1;\n").unwrap();
    let deps = OptimizedDeps::prepare(root, "0.0.1", input(), None);
    let before = deps.ready().await;
    assert!(!before.contains_key("latedep"), "not scanned, not bundled");
    let v0 = deps.browser_version();
    deps.register_missing("latedep");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let map = loop {
        let m = deps.ready().await;
        if m.contains_key("latedep") {
            break m;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the rerun never committed the discovered dep"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    };
    let v1 = deps.browser_version();
    assert_ne!(v1, v0, "a rerun bumps the browser version");
    assert!(
        map["latedep"].url.ends_with(&format!("?v={v1}")),
        "{}",
        map["latedep"].url
    );
    assert!(
        map["plaincjs"].url.ends_with(&format!("?v={v1}")),
        "known deps move to the new version too: {}",
        map["plaincjs"].url
    );

    // The next boot keeps the discovered dep and its version, no re-register.
    let after = OptimizedDeps::prepare(root, "0.0.1", input(), None);
    let warm2 = after.ready().await;
    assert!(
        warm2.contains_key("latedep"),
        "discovered deps persist in the manifest"
    );
    assert_eq!(after.browser_version(), v1, "persisted browser version");
}
