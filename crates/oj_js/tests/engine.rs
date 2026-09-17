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

#[tokio::test]
async fn deadline_times_out_a_parked_never_settling_call() {
    // The hang `terminate_execution` cannot reach: the promise never settles,
    // the event loop drains, and the scheduler parks. The park must be
    // bounded by the pending call's deadline.
    let root = app_root();
    std::fs::write(
        root.path().join("hang.mjs"),
        "export function hang() { return new Promise(() => {}); }\n\
         export function ok() { return 'alive'; }\n",
    )
    .unwrap();
    let mut config = EngineConfig::new(root.path());
    config.default_deadline = Some(Duration::from_millis(500));
    let engine = JsEngine::spawn(config).unwrap();
    let err = engine.call("hang.mjs", "hang", vec![]).await.unwrap_err();
    assert!(
        matches!(err, EngineError::Deadline),
        "expected Deadline, got {err:?}"
    );
    // The engine stays usable after the timed-out call.
    let value = engine.call("hang.mjs", "ok", vec![]).await.unwrap();
    assert_eq!(value, serde_json::json!("alive"));
}

#[tokio::test]
async fn deadline_terminates_an_infinite_loop_call() {
    // The busy-loop shape must keep working through the watchdog's
    // terminate_execution path.
    let root = app_root();
    std::fs::write(
        root.path().join("spin.mjs"),
        "export function spin() { for (;;) {} }\n\
         export function ok() { return 'alive'; }\n",
    )
    .unwrap();
    let mut config = EngineConfig::new(root.path());
    config.default_deadline = Some(Duration::from_millis(500));
    let engine = JsEngine::spawn(config).unwrap();
    let err = engine.call("spin.mjs", "spin", vec![]).await.unwrap_err();
    assert!(
        matches!(err, EngineError::Deadline),
        "expected Deadline, got {err:?}"
    );
    let value = engine.call("spin.mjs", "ok", vec![]).await.unwrap();
    assert_eq!(value, serde_json::json!("alive"));
}

#[tokio::test]
async fn no_deadline_call_parks_indefinitely_without_timing_out() {
    // SSR semantics: without a deadline a pending call parks for as long as
    // it takes, and the parked scheduler still serves other calls.
    let root = app_root();
    std::fs::write(
        root.path().join("park.mjs"),
        "export function park() { return new Promise(() => {}); }\n\
         export function ok() { return 'alive'; }\n",
    )
    .unwrap();
    let engine = engine(root.path());
    let parked = engine.call("park.mjs", "park", vec![]);
    tokio::pin!(parked);
    let raced = tokio::time::timeout(Duration::from_millis(700), parked.as_mut()).await;
    assert!(raced.is_err(), "a no-deadline call must stay parked");
    let value = engine.call("park.mjs", "ok", vec![]).await.unwrap();
    assert_eq!(value, serde_json::json!("alive"));
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

/// The ModuleHost seam: a host serving synthetic versioned modules, exercised
/// the way oj's SSR runner uses it (version-stamped specifiers, invalidation
/// by bumping versions up the importer chain, externals falling through to
/// byonm resolution).
mod host_seam {
    use super::*;
    use std::collections::HashMap;
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::sync::Mutex;

    use oj_js::HostFuture;
    use oj_js::HostModule;
    use oj_js::HostModuleType;
    use oj_js::HostResolved;
    use oj_js::ModuleHost;

    /// Serves modules from an in-memory map under `test://m/<id>?v=<version>`.
    /// Relative imports resolve to the current version of the named module;
    /// specifiers in `externals` defer to the engine's own node resolution.
    #[derive(Default)]
    struct TestHost {
        modules: Mutex<HashMap<String, String>>,
        versions: Mutex<HashMap<String, u64>>,
        externals: Mutex<HashSet<String>>,
    }

    impl TestHost {
        fn set(&self, id: &str, code: &str) {
            self.modules
                .lock()
                .unwrap()
                .insert(id.to_string(), code.to_string());
        }

        fn bump(&self, id: &str) {
            *self
                .versions
                .lock()
                .unwrap()
                .entry(id.to_string())
                .or_insert(0) += 1;
        }

        fn spec(&self, id: &str) -> String {
            let v = self.versions.lock().unwrap().get(id).copied().unwrap_or(0);
            format!("test://m/{id}?v={v}")
        }

        fn id_of(specifier: &str) -> Option<String> {
            let rest = specifier.strip_prefix("test://m/")?;
            Some(rest.split('?').next().unwrap_or(rest).to_string())
        }
    }

    impl ModuleHost for TestHost {
        fn resolve<'a>(
            &'a self,
            _importer: &'a str,
            specifier: &'a str,
        ) -> HostFuture<'a, Result<Option<HostResolved>, String>> {
            Box::pin(async move {
                if self.externals.lock().unwrap().contains(specifier) {
                    return Ok(Some(HostResolved::External(specifier.to_string())));
                }
                let id = specifier.trim_start_matches("./");
                if self.modules.lock().unwrap().contains_key(id) {
                    return Ok(Some(HostResolved::Url(self.spec(id))));
                }
                Ok(None)
            })
        }

        fn load<'a>(
            &'a self,
            specifier: &'a str,
        ) -> HostFuture<'a, Result<Option<HostModule>, String>> {
            Box::pin(async move {
                let Some(id) = TestHost::id_of(specifier) else {
                    return Ok(None);
                };
                match self.modules.lock().unwrap().get(&id) {
                    Some(code) => Ok(Some(HostModule {
                        code: code.clone(),
                        module_type: HostModuleType::JavaScript,
                    })),
                    None => Err(format!("host has no module \"{id}\"")),
                }
            })
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn synthetic_modules_resolve_and_load_through_the_host() {
        let root = app_root();
        let host = Arc::new(TestHost::default());
        host.set("dep", r#"export const word = "world";"#);
        host.set(
            "entry",
            r#"import { word } from "./dep";
               export function greet(name) { return `${name} ${word}`; }"#,
        );
        let engine =
            JsEngine::spawn_with_host(EngineConfig::new(root.path()), host.clone()).unwrap();
        let value = engine
            .call(
                host.spec("entry"),
                "greet",
                vec![serde_json::json!("hello")],
            )
            .await
            .unwrap();
        assert_eq!(value, serde_json::json!("hello world"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn version_bumps_reevaluate_changed_modules_and_keep_state() {
        let root = app_root();
        let host = Arc::new(TestHost::default());
        // Both modules count their evaluations in globals, so instance reuse
        // is observable: an unchanged specifier must reuse the instance (the
        // count stays), a bumped one must evaluate fresh.
        host.set(
            "dep",
            "export const stamp = (globalThis.__depRuns = (globalThis.__depRuns ?? 0) + 1);",
        );
        host.set(
            "entry",
            r#"import { stamp } from "./dep";
               const run = (globalThis.__entryRuns = (globalThis.__entryRuns ?? 0) + 1);
               export function stat() { return { stamp, run }; }"#,
        );
        let engine =
            JsEngine::spawn_with_host(EngineConfig::new(root.path()), host.clone()).unwrap();

        let first = engine
            .call(host.spec("entry"), "stat", vec![])
            .await
            .unwrap();
        assert_eq!(first, serde_json::json!({ "stamp": 1, "run": 1 }));

        // Same specifier: fully cached, nothing re-evaluates.
        let again = engine
            .call(host.spec("entry"), "stat", vec![])
            .await
            .unwrap();
        assert_eq!(again, serde_json::json!({ "stamp": 1, "run": 1 }));

        // The entry changed: its version (and only its version) bumps. The
        // fresh entry instance re-links against the untouched dep instance.
        host.bump("entry");
        let entry_only = engine
            .call(host.spec("entry"), "stat", vec![])
            .await
            .unwrap();
        assert_eq!(entry_only, serde_json::json!({ "stamp": 1, "run": 2 }));

        // The dep changed: the bump propagates up the importer chain (dep and
        // entry both), so both evaluate fresh.
        host.bump("dep");
        host.bump("entry");
        let both = engine
            .call(host.spec("entry"), "stat", vec![])
            .await
            .unwrap();
        assert_eq!(both, serde_json::json!({ "stamp": 2, "run": 3 }));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn external_specifiers_fall_through_to_byonm() {
        let root = app_root();
        let host = Arc::new(TestHost::default());
        host.externals.lock().unwrap().insert("oj-fixture".into());
        host.set(
            "entry",
            r#"import fixture from "oj-fixture";
               export function greet(name) { return fixture.greet(name); }"#,
        );
        let engine =
            JsEngine::spawn_with_host(EngineConfig::new(root.path()), host.clone()).unwrap();
        let value = engine
            .call(host.spec("entry"), "greet", vec![serde_json::json!("oj")])
            .await
            .unwrap();
        assert_eq!(value, serde_json::json!("hello oj"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_calls_share_the_isolate_without_serializing() {
        let root = app_root();
        let host = Arc::new(TestHost::default());
        // Two in-flight calls hand each other the baton: neither can finish
        // if the engine serializes jobs.
        host.set(
            "entry",
            r#"const state = (globalThis.__baton ??= { resolve: null, waiter: null });
               export function waitForBaton() {
                 return new Promise((resolve) => { state.resolve = resolve; });
               }
               export function handBaton(value) {
                 if (!state.resolve) return "no waiter";
                 state.resolve(value);
                 return "handed";
               }"#,
        );
        let engine = Arc::new(
            JsEngine::spawn_with_host(EngineConfig::new(root.path()), host.clone()).unwrap(),
        );
        let spec = host.spec("entry");
        let waiter = {
            let engine = engine.clone();
            let spec = spec.clone();
            tokio::spawn(async move { engine.call(spec, "waitForBaton", vec![]).await })
        };
        // Let the waiter's call reach its pending promise before handing off.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let handed = engine
            .call(spec, "handBaton", vec![serde_json::json!("relay")])
            .await
            .unwrap();
        assert_eq!(handed, serde_json::json!("handed"));
        let waited = waiter.await.unwrap().unwrap();
        assert_eq!(waited, serde_json::json!("relay"));
    }
}
