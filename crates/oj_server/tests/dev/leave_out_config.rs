// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! What the Start dev host's ssr containers read from the config: each
//! config-phase hook's work made once, as Vite's resolveConfig makes it, in
//! the host's own ssr container and in one leaving the reimplemented framework
//! plugins out.

use crate::{require_fixture, App};

/// The ssr container's external list for `plugins`, as a vite.config (the
/// fixture's Vite resolves it) or an oj plugins file.
async fn ssr_external(plugins: &str, vite: bool, leave_out: bool) -> serde_json::Value {
    ssr_facts(plugins, vite, leave_out).await["external"].clone()
}

/// The ssr container's startFacts for `plugins`.
async fn ssr_facts(plugins: &str, vite: bool, leave_out: bool) -> serde_json::Value {
    let (file, src) = if vite {
        (
            "vite.config.mjs",
            format!("export default {{ plugins: {plugins} }};\n"),
        )
    } else {
        ("oj.plugins.mjs", format!("export default {plugins};\n"))
    };
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"leave-out","private":true,"type":"module"}"#,
        ),
        (file, src.as_str()),
    ]);
    app.link_fixture("vite");
    let payload = serde_json::json!({
        "config": { "root": app.root.display().to_string(), "base": "/", "mode": "development", "command": "serve" },
        "env": { "command": "serve", "mode": "development" },
        "environment": { "name": "client", "mode": "dev" },
        "pluginsFormat": if vite { "vite" } else { "oj" },
        "ojStartMode": true,
    })
    .to_string();
    let host =
        oj_server::plugins::PluginHost::spawn(&app.root, &app.root.join(file), &payload, None)
            .await
            .expect("the Start dev host starts");
    let ssr = host
        .open_environment_container("ssr", leave_out, false)
        .await
        .expect("the ssr container opens");
    let facts = ssr.start_facts().await.expect("startFacts answers");
    host.shutdown();
    facts
}

/// A hook's in-place array edit is made once: Vite reaches one array as both
/// build.rollupOptions and build.rolldownOptions, and a leave-out container's
/// replay resolve already holds what the hooks did.
#[tokio::test]
async fn an_in_place_array_edit_is_made_once() {
    require_fixture!("vite");
    let plugins = r#"[
  { name: "tanstack-start-core:config", config: () => ({ build: { rollupOptions: { external: ["framework"] } } }) },
  {
    name: "pushes",
    config(config) {
      config.build ??= {};
      config.build.rollupOptions ??= {};
      config.build.rollupOptions.external ??= [];
      config.build.rollupOptions.external.push("user");
      config.build.rollupOptions.external.unshift("first");
    },
  },
]"#;
    for vite in [true, false] {
        assert_eq!(
            ssr_external(plugins, vite, true).await,
            serde_json::json!(["first", "user"]),
            "vite.config {vite}"
        );
    }
}

/// An environment's list is what Vite makes of it: the config hook's returned
/// environments.ssr partial, then configEnvironment's in-place push, each
/// once, whichever index the plugin sits at, in a leave-out ssr container and
/// in the host's own.
#[tokio::test]
async fn an_ssr_environment_list_matches_vite_at_every_plugin_index() {
    require_fixture!("vite");
    let pushes = r#"{
    name: "envpush",
    config: () => ({ environments: { ssr: { build: { rolldownOptions: { external: ["base"] } } } } }),
    configEnvironment(name, options) { if (name === "ssr") options.build.rolldownOptions.external.push("pushed"); },
  }"#;
    let filler = r#"{ name: "filler", config() {} }"#;
    for (at, plugins) in [
        (0, format!("[{pushes}, {filler}]")),
        (1, format!("[{filler}, {pushes}]")),
    ] {
        for vite in [true, false] {
            for leave_out in [true, false] {
                assert_eq!(
                    ssr_external(&plugins, vite, leave_out).await,
                    serde_json::json!(["base", "pushed"]),
                    "plugin at index {at}, vite.config {vite}, leave-out {leave_out}"
                );
            }
        }
    }
}

/// Vite awaits the client's buildStart before the server listens and starts
/// another environment on its own first use: under Start (which tolerates a
/// failed buildStart per plugin), a client buildStart that threw never keeps
/// the ssr environment's per-environment plugins from starting.
#[tokio::test]
async fn a_failed_client_build_start_leaves_the_ssr_start_running() {
    let log_dir = tempfile::tempdir().unwrap();
    let log = log_dir.path().join("hooks.log");
    let literal = serde_json::to_string(&log.display().to_string()).unwrap();
    let plugins = format!(
        r#"import fs from "node:fs";
export default [
  {{
    name: "fails-on-the-client",
    buildStart() {{ if (this.environment.name === "client") throw new Error("client-start-failed"); }},
  }},
  {{
    name: "per-environment",
    perEnvironmentStartEndDuringDev: true,
    buildStart() {{ fs.appendFileSync({literal}, "buildStart " + this.environment.name + "\n"); }},
  }},
];
"#
    );
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"start-build-start","private":true,"type":"module"}"#,
        ),
        ("oj.plugins.mjs", plugins.as_str()),
    ]);
    let payload = serde_json::json!({
        "config": { "root": app.root.display().to_string(), "base": "/", "mode": "development", "command": "serve" },
        "env": { "command": "serve", "mode": "development" },
        "environment": { "name": "client", "mode": "dev" },
        "pluginsFormat": "oj",
        "ojStartMode": true,
    })
    .to_string();
    let host = oj_server::plugins::PluginHost::spawn(
        &app.root,
        &app.root.join("oj.plugins.mjs"),
        &payload,
        None,
    )
    .await
    .expect("the Start dev host starts");
    assert!(host.build_start().await.is_err(), "the client start failed");
    let ssr = host
        .open_environment_container("ssr", false, false)
        .await
        .expect("the ssr container opens");
    let _ = ssr.build_start().await;
    host.shutdown();
    let text = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        text.lines().any(|l| l == "buildStart ssr"),
        "the ssr environment never started:\n{text}"
    );
}

/// An object-form alias a config hook returns lands in Vite's resolved alias
/// array once, as Vite's mergeAlias normalizes it: never merged again over
/// the resolve that already holds it (a malformed entry, a duplicate).
#[tokio::test]
async fn a_returned_alias_object_is_in_the_resolved_alias_once() {
    require_fixture!("vite");
    let plugins = r#"[{
  name: "aliases",
  config: () => ({ resolve: { alias: { "@c": "/abs/c" } } }),
  configEnvironment: (name) => (name === "ssr" ? { resolve: { conditions: ["ssr-only"] } } : null),
}]"#;
    for leave_out in [true, false] {
        let facts = ssr_facts(plugins, true, leave_out).await;
        let alias = facts["alias"].as_array().cloned().unwrap_or_default();
        assert!(
            alias
                .iter()
                .all(|a| a.get("find").is_some() && a.get("replacement").is_some()),
            "a malformed alias entry (leave-out {leave_out}): {alias:?}"
        );
        let ours = alias.iter().filter(|a| a["find"] == "@c").count();
        assert_eq!(ours, 1, "leave-out {leave_out}: {alias:?}");
    }
}
