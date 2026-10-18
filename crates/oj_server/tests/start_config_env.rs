// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The Start plugin bridge loads the app's vite.config through the REAL vite
//! the fixture brings, calling a function config with the configEnv Vite's
//! resolveConfig builds: `{ mode, command, isSsrBuild, isPreview }`, in that
//! order, isSsrBuild from the inline config alone (no build.ssr here). Skips
//! quietly when the fixture is not installed.

use std::path::Path;

const PROBE: &str = r#"import { loadPluginContainer } from "./start/vite-plugin-bridge.mjs";

export async function run(root) {
  await loadPluginContainer(root, { command: "serve" });
  await loadPluginContainer(root, { command: "build" });
}
"#;

const CONFIG: &str = r#"import { appendFileSync } from "node:fs";
import path from "node:path";
export default (configEnv) => {
  appendFileSync(path.join(__dirname, "seen.jsonl"), JSON.stringify(configEnv) + "\n");
  return {};
};
"#;

#[tokio::test(flavor = "multi_thread")]
async fn start_bridge_calls_a_function_config_with_vites_config_env() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let fixture_modules = repo.join("e2e/fixtures/start-app/node_modules");
    if !fixture_modules.join("vite").exists() {
        eprintln!("skipping: fixture vite not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::os::unix::fs::symlink(
        fixture_modules.canonicalize().unwrap(),
        root.join("node_modules"),
    )
    .unwrap();
    std::fs::write(
        root.join("package.json"),
        r#"{"name":"fx","type":"module"}"#,
    )
    .unwrap();
    std::fs::write(root.join("vite.config.mjs"), CONFIG).unwrap();
    oj_server::write_start_assets(&root.join("start")).unwrap();
    std::fs::write(root.join("probe.mjs"), PROBE).unwrap();

    let engine = oj_js::JsEngine::spawn(oj_js::EngineConfig::new(&root), None, None).unwrap();
    engine
        .call(
            root.join("probe.mjs").to_string_lossy(),
            "run",
            vec![serde_json::json!(root.to_string_lossy())],
            None,
        )
        .await
        .expect("the probe runs");

    assert_eq!(
        std::fs::read_to_string(root.join("seen.jsonl")).unwrap(),
        concat!(
            r#"{"mode":"development","command":"serve","isSsrBuild":false,"isPreview":false}"#,
            "\n",
            r#"{"mode":"production","command":"build","isSsrBuild":false,"isPreview":false}"#,
            "\n",
        ),
    );
}
