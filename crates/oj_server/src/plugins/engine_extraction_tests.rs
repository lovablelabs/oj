use super::*;

static ENGINE_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    ENGINE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn app(config: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("package.json"),
        r#"{"name":"fx","version":"1.0.0","type":"module"}"#,
    )
    .unwrap();
    std::fs::write(dir.path().join("vite.config.mjs"), config).unwrap();
    dir
}

#[test]
fn extraction_returns_values_and_caches_them() {
    let _g = lock();
    let dir = app(r#"export default { base: "/app/", server: { port: 5199 } };"#);
    let root = dir.path();
    let v = extract_vite_values_with(root, None, "serve", "development", true)
        .expect("a valid config extracts");
    assert_eq!(v.base.as_deref(), Some("/app/"));
    assert_eq!(v.port, Some(5199));

    // The run was cached under the engine-marked version tag, stderr field
    // and all, and the cached output parses back to the same values.
    let hit = extraction_store(root)
        .lookup(&root.join("vite.config.mjs"), "serve", "development")
        .expect("the extraction is cached");
    let json: serde_json::Value = serde_json::from_str(&hit.output).unwrap();
    assert_eq!(json["base"], "/app/");
    assert!(
        json.get("__stderr").is_none(),
        "the transcript is stored in its own field, not inside the output"
    );
    // The env files Vite would load for this mode are stamped, absent or
    // not, so creating one later is a cache miss.
    assert!(
        hit.deps.iter().any(|d| d.ends_with(".env.development")),
        "mode env files are stamped: {:?}",
        hit.deps
    );
}

#[test]
fn a_broken_config_is_none_not_an_empty_config() {
    let _g = lock();
    let dir = app("throw new Error('config exploded');\nexport default {};");
    let root = dir.path();
    assert!(
        extract_vite_values_with(root, None, "serve", "development", true).is_none(),
        "a config that fails to evaluate must never parse as empty values"
    );
    assert!(
        extraction_store(root)
            .lookup(&root.join("vite.config.mjs"), "serve", "development")
            .is_none(),
        "a failed extraction is never cached"
    );
    // ...and the adopt seam surfaces it as the load error Vite gives.
    let mut config = oj_config::OjConfig::default();
    let err = adopt_vite_config_values(&mut config, root, None, "serve", "development")
        .expect_err("a present-but-broken vite.config is an error");
    assert!(err.contains("failed to load config"), "{err}");
}

#[test]
fn config_imports_are_recorded_as_deps() {
    let _g = lock();
    let dir = app(r#"import { base } from "./base.config.mjs";
export default { base };"#);
    let root = dir.path();
    std::fs::write(
        root.join("base.config.mjs"),
        "export const base = \"/dep/\";\n",
    )
    .unwrap();
    let v = extract_vite_values_with(root, None, "serve", "development", true).unwrap();
    assert_eq!(v.base.as_deref(), Some("/dep/"));
    let hit = extraction_store(root)
        .lookup(&root.join("vite.config.mjs"), "serve", "development")
        .expect("cached");
    assert!(
        hit.deps.iter().any(|d| d.ends_with("base.config.mjs")),
        "the config's own imports invalidate the cache: {:?}",
        hit.deps
    );
}

#[test]
fn truncated_observed_reads_serve_but_never_cache() {
    let _g = lock();
    // Cap the recorder at one path; the config reads two .json files, so the
    // dep stamp is incomplete and the result must not be cached.
    std::env::set_var("OJ_OBSERVED_READS_MAX", "1");
    let dir = app(r#"import fs from "node:fs";
const a = JSON.parse(fs.readFileSync(new URL("./a.json", import.meta.url), "utf8"));
const b = JSON.parse(fs.readFileSync(new URL("./b.json", import.meta.url), "utf8"));
export default { base: a.base + b.base };"#);
    let root = dir.path();
    std::fs::write(root.join("a.json"), r#"{"base":"/a"}"#).unwrap();
    std::fs::write(root.join("b.json"), r#"{"base":"/b"}"#).unwrap();
    let result = extract_vite_values_with(root, None, "serve", "development", true);
    std::env::remove_var("OJ_OBSERVED_READS_MAX");
    let v = result.expect("the result is still served");
    assert_eq!(v.base.as_deref(), Some("/a/b"));
    assert!(
        extraction_store(root)
            .lookup(&root.join("vite.config.mjs"), "serve", "development")
            .is_none(),
        "an extraction with a truncated dep stamp must not be cached"
    );
}

#[test]
fn a_config_that_never_finishes_is_terminated_at_the_deadline() {
    let _g = lock();
    let dir = app("await new Promise(() => {});\nexport default {};");
    let root = dir.path();
    let started = std::time::Instant::now();
    let result = extract_vite_values_timed(
        root,
        None,
        "serve",
        "development",
        true,
        extraction_timeout_from(Some("2")),
    );
    assert!(result.is_none(), "a wedged config evaluation is a failure");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(30),
        "the deadline must end the wait, not the config's leisure"
    );
}

#[test]
fn a_hook_started_interval_does_not_outlive_the_extraction() {
    let _g = lock();
    // A config that leaves timers behind (the TanStack route-generator shape)
    // must not stall the caller; the engine dies with them at drop.
    let dir = app("setInterval(() => {}, 1000);\nexport default { base: \"/live/\" };");
    let root = dir.path();
    let started = std::time::Instant::now();
    let v = extract_vite_values_with(root, None, "serve", "development", true).unwrap();
    assert_eq!(v.base.as_deref(), Some("/live/"));
    assert!(
        started.elapsed() < std::time::Duration::from_secs(30),
        "a live timer must not hold the extraction open"
    );
}

#[test]
fn stderr_prints_from_config_code_travel_in_the_transcript() {
    let _g = lock();
    let dir = app(r#"console.error("plugin says hi");
process.stderr.write("direct stderr write\n");
console.log("stdout is swallowed");
export default { base: "/loud/" };"#);
    let root = dir.path();
    let v = extract_vite_values_with(root, None, "serve", "development", true).unwrap();
    assert_eq!(v.base.as_deref(), Some("/loud/"));
    let hit = extraction_store(root)
        .lookup(&root.join("vite.config.mjs"), "serve", "development")
        .expect("cached");
    assert!(hit.stderr.contains("plugin says hi"), "{}", hit.stderr);
    assert!(hit.stderr.contains("direct stderr write"), "{}", hit.stderr);
    assert!(
        !hit.stderr.contains("stdout is swallowed"),
        "stdout prints are dropped, as the old subprocess capture dropped them: {}",
        hit.stderr
    );
}

#[test]
fn config_env_writes_do_not_leak_into_the_oj_process() {
    let _g = lock();
    // The engine must shadow process.env (Vite's own NODE_ENV dance runs on
    // every extraction).
    let dir = app(r#"process.env.OJ_EXTRACT_LEAK_PROBE = "leaked";
export default { base: "/env/" };"#);
    let root = dir.path();
    let v = extract_vite_values_with(root, None, "serve", "development", true).unwrap();
    assert_eq!(v.base.as_deref(), Some("/env/"));
    assert!(
        std::env::var("OJ_EXTRACT_LEAK_PROBE").is_err(),
        "a config's env write must die with its extraction"
    );
}

// TS-config fallback (no vite installed): the extractor bundles the config
// with the app's esbuild. Skips quietly when the fixture has no node_modules.
#[test]
fn a_ts_config_without_vite_loads_through_the_esbuild_fallback() {
    let _g = lock();
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let esbuild = repo.join("e2e/fixtures/start-app/node_modules/esbuild");
    if !esbuild.exists() {
        eprintln!("skipping: fixture esbuild not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("package.json"),
        r#"{"name":"fx","version":"1.0.0","type":"module","dependencies":{"esbuild":"*"}}"#,
    )
    .unwrap();
    std::fs::create_dir_all(root.join("node_modules")).unwrap();
    std::os::unix::fs::symlink(&esbuild, root.join("node_modules/esbuild")).unwrap();
    let scoped = repo.join("e2e/fixtures/start-app/node_modules/@esbuild");
    if scoped.exists() {
        std::os::unix::fs::symlink(&scoped, root.join("node_modules/@esbuild")).unwrap();
    }
    std::fs::write(
        root.join("shared.ts"),
        "export const port: number = 5321;\n",
    )
    .unwrap();
    std::fs::write(
        root.join("vite.config.ts"),
        "import { port } from \"./shared\";\nexport default { base: \"/ts/\" as const, server: { port } };\n",
    )
    .unwrap();
    let v = extract_vite_values_with(root, None, "serve", "development", true)
        .expect("the TS config loads through the esbuild fallback");
    assert_eq!(v.base.as_deref(), Some("/ts/"));
    assert_eq!(v.port, Some(5321));
    // The bundle's metafile names the config's imports as deps.
    let hit = extraction_store(root)
        .lookup(&root.join("vite.config.ts"), "serve", "development")
        .expect("cached");
    assert!(
        hit.deps.iter().any(|d| d.ends_with("shared.ts")),
        "esbuild metafile inputs are stamped: {:?}",
        hit.deps
    );
}

// Checked at the exact production seam: a module on the engine spawns a real
// child process (as esbuild's JS API does) and reads it back.
#[test]
fn engine_jobs_can_spawn_child_processes() {
    let _g = lock();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("package.json"),
        r#"{"name":"fx","version":"1.0.0","type":"module"}"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("spawn-job.mjs"),
        r#"import { execFileSync, spawn } from "node:child_process";
export async function run() {
  const sync = execFileSync("/bin/echo", ["sync-child"], { encoding: "utf8" }).trim();
  const child = spawn("/bin/echo", ["piped-child"]);
  let piped = "";
  child.stdout.on("data", (d) => { piped += d.toString(); });
  const code = await new Promise((resolve) => child.on("close", resolve));
  return { sync, piped: piped.trim(), code };
}
"#,
    )
    .unwrap();
    if !std::path::Path::new("/bin/echo").exists() {
        return; // not a unix-y machine: the seam under test cannot run
    }
    let out = run_engine_job(
        dir.path(),
        &dir.path().join("spawn-job.mjs"),
        "run",
        serde_json::json!({}),
        std::time::Duration::from_secs(30),
    )
    .expect("child_process must work under the embedded engine");
    assert_eq!(out["sync"], "sync-child");
    assert_eq!(out["piped"], "piped-child");
    assert_eq!(out["code"], 0);
}

// The child talks through this envelope; a variant not surviving the round
// trip would turn a child's deadline or JS error into a generic boot failure.
#[test]
fn engine_job_envelope_round_trips_every_outcome() {
    let outcomes: Vec<Result<serde_json::Value, oj_js::EngineError>> = vec![
        Ok(serde_json::json!({ "a": [1, "two"] })),
        Err(oj_js::EngineError::Boot("no engine".into())),
        Err(oj_js::EngineError::Js("TypeError: boom".into())),
        Err(oj_js::EngineError::MemoryLimit),
        Err(oj_js::EngineError::Deadline),
        Err(oj_js::EngineError::Closed),
    ];
    for outcome in outcomes {
        let back = engine_job_outcome(engine_job_envelope(&outcome));
        match (&outcome, &back) {
            (Ok(a), Ok(b)) => assert_eq!(a, b),
            (Err(a), Err(b)) => assert_eq!(a.to_string(), b.to_string()),
            _ => panic!("outcome {outcome:?} came back as {back:?}"),
        }
    }
}
