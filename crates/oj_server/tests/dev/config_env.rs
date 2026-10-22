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

const BUILD_CONFIG: &str = r#"import { appendFileSync } from "node:fs";
import path from "node:path";
const log = (what) => appendFileSync(path.join(__dirname, "seen.jsonl"), JSON.stringify(what) + "\n");
export default (configEnv) => {
  log({ configEnv });
  return {
    plugins: [{
      name: "env-probe",
      configEnvironment(name, _options, env) {
        log({ name, isSsrBuild: env.isSsrBuild });
      },
    }],
  };
};
"#;

/// A build resolves one config for every environment (Vite's builder), so
/// the ssr environment's own plugin host still sees `isSsrBuild: false`,
/// as its config function and every `configEnvironment` call do under Vite:
/// isSsrBuild comes from the inline config's build.ssr alone.
#[tokio::test(flavor = "multi_thread")]
async fn an_ssr_build_host_sees_vites_config_env() {
    require_fixture!("vite");
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"buildenv","private":true,"type":"module"}"#,
        ),
        ("vite.config.mjs", BUILD_CONFIG),
    ]);
    app.link_fixture("vite");
    let payload = serde_json::json!({
        "config": { "root": app.root.display().to_string(), "base": "/", "mode": "production", "command": "build" },
        "env": { "command": "build", "mode": "production" },
        "environment": { "name": "ssr", "mode": "build" },
        "pluginsFormat": "vite",
    })
    .to_string();
    let host = oj_server::plugins::PluginHost::spawn(
        &app.root,
        &app.root.join("vite.config.mjs"),
        &payload,
        None,
    )
    .await
    .expect("the ssr build host starts");
    let names = host.plugin_names().await;
    host.shutdown();
    assert!(
        names.iter().any(|n| n == "env-probe"),
        "the config's plugin did not load: {names:?}"
    );

    let seen = std::fs::read_to_string(app.root.join("seen.jsonl")).unwrap();
    let lines: Vec<&str> = seen.lines().collect();
    assert!(
        lines.contains(
            &r#"{"configEnv":{"mode":"production","command":"build","isSsrBuild":false,"isPreview":false}}"#
        ),
        "the config function saw another env:\n{seen}"
    );
    for name in ["client", "ssr"] {
        let want = format!(r#"{{"name":"{name}","isSsrBuild":false}}"#);
        assert!(
            lines.contains(&want.as_str()),
            "configEnvironment({name}) did not see isSsrBuild false:\n{seen}"
        );
    }
    assert!(
        !seen.contains("\"isSsrBuild\":true"),
        "a hook saw isSsrBuild true:\n{seen}"
    );
}
