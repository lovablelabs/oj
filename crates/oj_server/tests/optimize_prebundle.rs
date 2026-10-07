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

    // Vite's registerMissingImport: a dep the scan never saw registers and is
    // rewritten to its FUTURE optimized URL at once; the debounced rerun
    // bundles it. latedep shares nothing with plaincjs, so the committed
    // plaincjs entry is byte-identical and the commit must NOT reload: the
    // browser version stays, every served URL stays valid (Vite's needsReload
    // split).
    let write_dep = |name: &str, code: &str, esm: bool| {
        let dir = root.join("node_modules").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let module_type = if esm { r#""type":"module","# } else { "" };
        std::fs::write(
            dir.join("package.json"),
            format!(r#"{{"name":"{name}","version":"1.0.0",{module_type}"main":"index.js"}}"#),
        )
        .unwrap();
        std::fs::write(dir.join("index.js"), code).unwrap();
    };
    // An ESM dep bundles standalone: the committed plaincjs entry stays
    // byte-identical, so this commit must take the no-reload path.
    write_dep("latedep", "export const l = 1;\n", true);
    let deps = OptimizedDeps::prepare(root, "0.0.1", input(), None);
    let before = deps.ready().await;
    assert!(!before.contains_key("latedep"), "not scanned, not bundled");
    let v0 = deps.browser_version();
    let plaincjs_v0 = before["plaincjs"].url.clone();
    let provisional = deps
        .register_missing("latedep", false)
        .expect("registration returns the future meta");
    assert!(
        provisional.url.starts_with("/@oj-deps/latedep.mjs?v="),
        "the optimized URL is known before bundling: {}",
        provisional.url
    );
    let wait_for = |pred: Box<dyn Fn(&oj_server::optimize::DepMap) -> bool>| {
        let deps = &deps;
        async move {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
            loop {
                let m = deps.ready().await;
                if pred(&m) {
                    break m;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "the rerun never committed"
                );
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    };
    let map = wait_for(Box::new(|m| {
        m.get("latedep").is_some_and(|l| !l.file_hash.is_empty())
    }))
    .await;
    assert_eq!(
        deps.browser_version(),
        v0,
        "unchanged served entries: no version bump, no reload"
    );
    assert_eq!(
        map["latedep"].url, provisional.url,
        "the committed URL is the one importers already carry"
    );
    assert_eq!(map["plaincjs"].url, plaincjs_v0, "known URLs stay valid");
    assert!(
        !map["latedep"].needs_interop,
        "bundle agrees with the guess"
    );

    // A discovered dep that drags plaincjs into a shared chunk rewrites the
    // served plaincjs entry: that commit must take the reload path and move
    // every URL to the batch version.
    write_dep(
        "latedep2",
        "const p = require(\"plaincjs\");\nexports.both = p.a;\n",
        false,
    );
    deps.register_missing("latedep2", true)
        .expect("second registration");
    let map = wait_for(Box::new(|m| {
        m.get("latedep2").is_some_and(|l| !l.file_hash.is_empty())
    }))
    .await;
    let v2 = deps.browser_version();
    assert_ne!(v2, v0, "a changed served entry bumps the version");
    assert!(
        map["plaincjs"].url.ends_with(&format!("?v={v2}"))
            && map["latedep"].url.ends_with(&format!("?v={v2}"))
            && map["latedep2"].url.ends_with(&format!("?v={v2}")),
        "every URL moves to the batch version on reload commits"
    );

    // The next boot keeps the discovered deps and versions, no re-register.
    let after = OptimizedDeps::prepare(root, "0.0.1", input(), None);
    let warm2 = after.ready().await;
    assert!(
        warm2.contains_key("latedep") && warm2.contains_key("latedep2"),
        "discovered deps persist in the manifest"
    );
    assert_eq!(after.browser_version(), v2, "persisted browser version");
}
