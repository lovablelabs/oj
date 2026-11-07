// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Its own test binary: `consume_only` is process-wide.

use std::path::Path;

use oj_js::EngineConfig;
use oj_js::JsEngine;

// An engine job's parent waits on its exit, so the child creates no code
// cache: not the ESM entry, not the CJS dependency's bytecode, not its
// export analysis. Writing them at exit would put the cost back on the
// parent's critical path.
#[tokio::test]
async fn a_consume_only_process_compiles_without_creating_entries() {
    oj_js::code_cache::consume_only();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("package.json"),
        r#"{"name":"fixture-app","version":"1.0.0","dependencies":{"oj-fixture":"1.0.0"}}"#,
    )
    .unwrap();
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/oj-fixture");
    let dst = root.path().join("node_modules/oj-fixture");
    std::fs::create_dir_all(&dst).unwrap();
    for entry in std::fs::read_dir(&src).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), dst.join(entry.file_name())).unwrap();
    }
    std::fs::write(
        root.path().join("main.mjs"),
        r#"
        import fixture from "oj-fixture";
        export function run() { return fixture.greet("job"); }
        "#,
    )
    .unwrap();
    let cache_dir = root.path().join("cc");
    let mut config = EngineConfig::new(root.path());
    config.code_cache_dir = Some(cache_dir.clone());
    let engine = JsEngine::spawn(config, None, None).unwrap();
    let value = engine.call("main.mjs", "run", vec![], None).await.unwrap();
    assert_eq!(value, serde_json::json!("hello job"));
    drop(engine);
    oj_js::code_cache::flush_at_exit();
    let entries = std::fs::read_dir(&cache_dir).map_or(0, |d| d.count());
    assert_eq!(
        entries, 0,
        "a consume-only process wrote code cache entries"
    );
}
