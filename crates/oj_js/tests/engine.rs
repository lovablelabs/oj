// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use oj_js::EngineConfig;
use oj_js::EngineError;
use oj_js::EvalInput;
use oj_js::JsEngine;

/// Materializes an app root in a temp dir: a package.json plus the hand
/// written fixture package copied into node_modules (the fixture lives
/// outside a literal node_modules directory because the repo gitignores
/// those).
fn app_root() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("package.json"),
        r#"{"name":"fixture-app","version":"1.0.0","dependencies":{"oj-fixture":"1.0.0"}}"#,
    )
    .unwrap();
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/oj-fixture");
    let dst = dir.path().join("node_modules/oj-fixture");
    std::fs::create_dir_all(&dst).unwrap();
    for entry in std::fs::read_dir(&src).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), dst.join(entry.file_name())).unwrap();
    }
    dir
}

fn engine(root: &Path) -> JsEngine {
    JsEngine::spawn(EngineConfig::new(root)).unwrap()
}

const fn assert_send_sync<T: Send + Sync>() {}
const _: () = assert_send_sync::<JsEngine>();

#[tokio::test]
async fn eval_source_completes_with_default_export() {
    let root = app_root();
    let engine = engine(root.path());
    let value = engine
        .eval(EvalInput::Source("export default 1 + 1;".into()))
        .await
        .unwrap();
    assert_eq!(value, serde_json::json!(2));
}

#[tokio::test]
async fn eval_runs_event_loop_to_completion() {
    let root = app_root();
    let engine = engine(root.path());
    let value = engine
        .eval(EvalInput::Source(
            "export default await new Promise((resolve) => setTimeout(() => resolve('done'), 10));"
                .into(),
        ))
        .await
        .unwrap();
    assert_eq!(value, serde_json::json!("done"));
}

#[tokio::test]
async fn node_builtins_work() {
    let root = app_root();
    let engine = engine(root.path());
    let value = engine
        .eval(EvalInput::Source(
            r#"
            import { join, basename } from "node:path";
            import { tmpdir } from "node:os";
            import { writeFileSync, readFileSync, rmSync } from "node:fs";
            const file = join(tmpdir(), `oj_js_builtin_test_${Date.now()}.txt`);
            writeFileSync(file, "roundtrip:" + basename(file));
            const text = readFileSync(file, "utf8");
            rmSync(file);
            export default text.startsWith("roundtrip:");
            "#
            .into(),
        ))
        .await
        .unwrap();
    assert_eq!(value, serde_json::json!(true));
}

#[tokio::test]
async fn bare_import_resolves_from_node_modules() {
    let root = app_root();
    let engine = engine(root.path());
    // ESM import of a CommonJS package: exercises byonm resolution plus the
    // CJS-to-ESM translation.
    let value = engine
        .eval(EvalInput::Source(
            r#"
            import fixture from "oj-fixture";
            export default fixture.greet("oj");
            "#
            .into(),
        ))
        .await
        .unwrap();
    assert_eq!(value, serde_json::json!("hello oj"));
}

#[tokio::test]
async fn create_require_resolves_from_node_modules() {
    let root = app_root();
    let engine = engine(root.path());
    let value = engine
        .eval(EvalInput::Source(
            r#"
            import { createRequire } from "node:module";
            const require = createRequire(import.meta.url);
            const fixture = require("oj-fixture");
            export default fixture.answer;
            "#
            .into(),
        ))
        .await
        .unwrap();
    assert_eq!(value, serde_json::json!(42));
}

#[tokio::test]
async fn eval_path_and_call_work() {
    let root = app_root();
    std::fs::write(
        root.path().join("job.mjs"),
        r#"
        import fixture from "oj-fixture";
        export function greetTwice(name) {
            return fixture.greet(name) + "!" + fixture.greet(name);
        }
        export async function asyncAdd(a, b) {
            return a + b;
        }
        export default "loaded";
        "#,
    )
    .unwrap();
    let engine = engine(root.path());

    let value = engine
        .eval(EvalInput::Path(PathBuf::from("job.mjs")))
        .await
        .unwrap();
    assert_eq!(value, serde_json::json!("loaded"));

    let value = engine
        .call("job.mjs", "greetTwice", vec![serde_json::json!("oj")])
        .await
        .unwrap();
    assert_eq!(value, serde_json::json!("hello oj!hello oj"));

    let value = engine
        .call(
            "job.mjs",
            "asyncAdd",
            vec![serde_json::json!(20), serde_json::json!(22)],
        )
        .await
        .unwrap();
    assert_eq!(value, serde_json::json!(42));
}

#[tokio::test]
async fn js_errors_are_reported_not_fatal() {
    let root = app_root();
    let engine = engine(root.path());
    let err = engine
        .eval(EvalInput::Source("throw new Error('boom');".into()))
        .await
        .unwrap_err();
    match err {
        EngineError::Js(message) => assert!(message.contains("boom"), "unexpected: {message}"),
        other => panic!("expected Js error, got {other:?}"),
    }
    // The engine stays usable.
    let value = engine
        .eval(EvalInput::Source("export default 'still alive';".into()))
        .await
        .unwrap();
    assert_eq!(value, serde_json::json!("still alive"));
}

#[tokio::test]
async fn memory_cap_terminates_cleanly() {
    let root = app_root();
    let mut config = EngineConfig::new(root.path());
    config.memory_limit_bytes = Some(128 * 1024 * 1024);
    let engine = JsEngine::spawn(config).unwrap();
    let err = engine
        .eval(EvalInput::Source(
            r#"
            const chunks = [];
            for (;;) chunks.push(new Array(1024 * 1024).fill(Math.random()));
            "#
            .into(),
        ))
        .await
        .unwrap_err();
    assert!(
        matches!(err, EngineError::MemoryLimit),
        "expected MemoryLimit, got {err:?}"
    );
}

#[tokio::test]
async fn deadline_terminates_infinite_loop() {
    let root = app_root();
    let engine = engine(root.path());
    let err = engine
        .eval_with_deadline(
            EvalInput::Source("for (;;) {}".into()),
            Some(Duration::from_millis(500)),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, EngineError::Deadline),
        "expected Deadline, got {err:?}"
    );
    // The engine stays usable after a terminated job.
    let value = engine
        .eval(EvalInput::Source("export default 7;".into()))
        .await
        .unwrap();
    assert_eq!(value, serde_json::json!(7));
}

#[test]
fn two_engines_run_concurrently() {
    // init_platform must hold across engines spawned from different threads.
    let run = || {
        let root = app_root();
        let engine = engine(root.path());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let value = rt
            .block_on(engine.eval(EvalInput::Source(
                "export default [1, 2, 3].map((n) => n * 2);".into(),
            )))
            .unwrap();
        assert_eq!(value, serde_json::json!([2, 4, 6]));
    };
    let a = std::thread::spawn(run);
    let b = std::thread::spawn(run);
    a.join().unwrap();
    b.join().unwrap();
}

/// Loads a real napi addon through createRequire. Needs npm and network, so it
/// is ignored by default; run it with:
///
/// ```sh
/// cargo test -p oj_js -- --ignored napi
/// ```
///
/// Loading a `.node` addon resolves napi_* symbols against the running
/// executable, so this doubles as a check that the test binary exports them
/// (the `oj` binary gets them explicitly via `deno_napi::print_linker_flags`
/// in crates/oj/build.rs).
#[tokio::test]
#[ignore = "needs npm + network; see doc comment"]
async fn napi_addon_loads() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("package.json"),
        r#"{"name":"napi-fixture","version":"1.0.0"}"#,
    )
    .unwrap();
    let status = std::process::Command::new("npm")
        .args(["install", "lightningcss@1"])
        .current_dir(root.path())
        .status()
        .expect("npm must be installed to run this test");
    assert!(status.success(), "npm install failed");

    let engine = engine(root.path());
    let value = engine
        .eval(EvalInput::Source(
            r#"
            import { createRequire } from "node:module";
            const require = createRequire(import.meta.url);
            const css = require("lightningcss");
            const out = css.transform({
                filename: "t.css",
                code: new TextEncoder().encode(".a { color: #ff0000; }"),
                minify: true,
            });
            export default new TextDecoder().decode(out.code);
            "#
            .into(),
        ))
        .await
        .unwrap();
    assert_eq!(value, serde_json::json!(".a{color:red}"));
}
