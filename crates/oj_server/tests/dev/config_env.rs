// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The app's vite.config, loaded through the real Vite, sees what `vite dev`
//! gives it. Vite's resolveConfig (config.ts) sets `NODE_ENV` to the default
//! when unset, then builds `configEnv = { mode, command, isSsrBuild,
//! isPreview }`, and only then calls loadConfigFromFile, which calls a
//! function config with that configEnv. oj evaluates the config twice (the
//! value extraction and the plugin host, whose engine imports vite ahead of
//! the config); every evaluation must match.

use crate::{require_fixture, App};

const CONFIG: &str = r#"import { appendFileSync } from "node:fs";
import path from "node:path";
export default (configEnv) => {
  const seen = { configEnv, nodeEnv: process.env.NODE_ENV ?? null };
  appendFileSync(path.join(__dirname, "seen.jsonl"), JSON.stringify(seen) + "\n");
  return {
    plugins: [{
      name: "seen-probe",
      transform(code, id) {
        if (!id.endsWith("main.js")) return null;
        return code + "\nexport const __seen = " + JSON.stringify(seen) + ";\n";
      },
    }],
  };
};
"#;

#[tokio::test(flavor = "multi_thread")]
async fn function_config_gets_vites_config_env_and_node_env() {
    require_fixture!("vite");
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"configenv","private":true,"type":"module"}"#,
        ),
        (
            "index.html",
            r#"<!doctype html><html><head></head><body><script type="module" src="/src/main.js"></script></body></html>"#,
        ),
        ("src/main.js", "export const v = 1;\n"),
        ("vite.config.mjs", CONFIG),
    ]);
    app.link_fixture("vite");
    let server = app.serve().await;

    // Vite's object literal order: `{ mode, command, isSsrBuild, isPreview }`.
    let expected = r#"{"configEnv":{"mode":"development","command":"serve","isSsrBuild":false,"isPreview":false},"nodeEnv":"development"}"#;
    let module = server.text("/src/main.js").await;
    assert!(
        module.contains("__seen"),
        "the config's plugin did not run:\n{module}"
    );
    let seen = std::fs::read_to_string(app.root.join("seen.jsonl")).unwrap();
    // The extraction's evaluation and the plugin host's, at least.
    assert!(
        seen.lines().count() >= 2,
        "the config ran fewer than twice:\n{seen}"
    );
    for line in seen.lines() {
        assert_eq!(line, expected, "a config evaluation saw another env");
    }
}
