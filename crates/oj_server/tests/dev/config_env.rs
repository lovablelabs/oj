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

const PROMISED_TANSTACK_CONFIG: &str = r#"import { appendFileSync } from "node:fs";
import path from "node:path";
const ran = (name) => ({
  name,
  config() {
    appendFileSync(path.join(__dirname, "config-ran.txt"), name + "\n");
  },
});
export default {
  plugins: [
    ran("user-probe"),
    Promise.resolve([ran("tanstack-start-core:config"), Promise.resolve(ran("tanstack:router-generator"))]),
  ],
};
"#;

/// A Start bundle host resolves the config through Vite without the plugins
/// oj reimplements; Vite asyncFlattens `plugins`, so TanStack's plugins handed
/// in as `Promise.resolve([...])` are left out too: their config hooks never
/// run and they never reach the host's plugin list.
#[tokio::test(flavor = "multi_thread")]
async fn start_bundle_host_leaves_out_promise_wrapped_tanstack_plugins() {
    require_fixture!("vite");
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"promised","private":true,"type":"module"}"#,
        ),
        ("vite.config.mjs", PROMISED_TANSTACK_CONFIG),
    ]);
    app.link_fixture("vite");
    let payload = serde_json::json!({
        "config": { "root": app.root.display().to_string(), "base": "/", "mode": "production", "command": "build" },
        "env": { "command": "build", "mode": "production" },
        "environment": { "name": "client", "mode": "build" },
        "pluginsFormat": "vite",
        "ojStartMode": true,
        "ojBundleHost": true,
    })
    .to_string();
    let host = oj_server::plugins::PluginHost::spawn(
        &app.root,
        &app.root.join("vite.config.mjs"),
        &payload,
        None,
    )
    .await
    .expect("the bundle host starts");
    let names = host.plugin_names().await;
    host.shutdown();
    // A plugin list entry that is not a plugin reads as a nameless one.
    assert!(
        names.iter().any(|n| n == "user-probe") && !names.iter().any(|n| n.is_empty()),
        "the host's plugins: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.starts_with("tanstack")),
        "a TanStack plugin is hosted: {names:?}"
    );
    // The user's config hook ran once, inside Vite's resolveConfig.
    let ran = std::fs::read_to_string(app.root.join("config-ran.txt")).unwrap_or_default();
    assert_eq!(ran, "user-probe\n", "the config hooks that ran");
}

/// The Start SSR host leaves the same plugins out of its Vite resolve as a
/// bundle host: TanStack's config hooks (the route generator's among them)
/// do work oj's own Start pipeline does, so only the user's hook runs, once.
#[tokio::test(flavor = "multi_thread")]
async fn start_ssr_host_leaves_tanstack_config_hooks_out_of_its_resolve() {
    require_fixture!("vite");
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"ssrresolve","private":true,"type":"module"}"#,
        ),
        ("vite.config.mjs", PROMISED_TANSTACK_CONFIG),
    ]);
    app.link_fixture("vite");
    let payload = serde_json::json!({
        "config": { "root": app.root.display().to_string(), "base": "/", "mode": "development", "command": "serve" },
        "env": { "command": "serve", "mode": "development" },
        "environment": { "name": "ssr", "mode": "dev" },
        "pluginsFormat": "vite",
        "ojStartMode": true,
    })
    .to_string();
    let host = oj_server::plugins::PluginHost::spawn(
        &app.root,
        &app.root.join("vite.config.mjs"),
        &payload,
        None,
    )
    .await
    .expect("the ssr host starts");
    let names = host.plugin_names().await;
    host.shutdown();
    assert!(
        names.iter().any(|n| n == "user-probe"),
        "the host's plugins: {names:?}"
    );
    let ran = std::fs::read_to_string(app.root.join("config-ran.txt")).unwrap_or_default();
    assert_eq!(ran, "user-probe\n", "the config hooks that ran");
}

/// Vite's build reads `build.rolldownOptions.plugins` through
/// asyncFlatten(arraify(..)) (build.ts): a lone plugin on a Worker
/// environment is a one-entry list in the Start facts, not a throw.
#[tokio::test(flavor = "multi_thread")]
async fn cloudflare_facts_read_a_lone_rolldown_plugin() {
    require_fixture!("vite");
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"lonecf","private":true,"type":"module"}"#,
        ),
        (
            "vite.config.mjs",
            r#"export default {
  environments: {
    worker: {
      resolve: { builtins: ["cloudflare:workers"] },
      build: { rolldownOptions: { plugins: { name: "builtin:esm-external-require", _options: { external: ["a"] } } } },
    },
  },
};
"#,
        ),
    ]);
    app.link_fixture("vite");
    let payload = serde_json::json!({
        "config": { "root": app.root.display().to_string(), "base": "/", "mode": "production", "command": "build" },
        "env": { "command": "build", "mode": "production" },
        "environment": { "name": "client", "mode": "build" },
        "pluginsFormat": "vite",
        "ojStartMode": true,
        "ojBundleHost": true,
    })
    .to_string();
    let host = oj_server::plugins::PluginHost::spawn(
        &app.root,
        &app.root.join("vite.config.mjs"),
        &payload,
        None,
    )
    .await
    .expect("the bundle host starts");
    let facts = host.start_facts().await;
    host.shutdown();
    let facts = facts.expect("start facts are served");
    assert_eq!(
        facts["cloudflare"]["rolldownPlugins"],
        serde_json::json!([{ "name": "builtin:esm-external-require", "options": { "external": ["a"] } }]),
        "{facts}"
    );
}

/// The same read awaits: a Worker's builtin handed in as
/// `Promise.resolve([..])` is in the facts with its options, as Vite's
/// build's asyncFlatten gives it to rolldown.
#[tokio::test(flavor = "multi_thread")]
async fn cloudflare_facts_await_a_promised_rolldown_plugin() {
    require_fixture!("vite");
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"promisedcf","private":true,"type":"module"}"#,
        ),
        (
            "vite.config.mjs",
            r#"export default {
  environments: {
    worker: {
      resolve: { builtins: ["cloudflare:workers"] },
      build: {
        rolldownOptions: {
          plugins: [Promise.resolve([{ name: "builtin:esm-external-require", _options: { external: ["b"] } }]), null],
        },
      },
    },
  },
};
"#,
        ),
    ]);
    app.link_fixture("vite");
    let payload = serde_json::json!({
        "config": { "root": app.root.display().to_string(), "base": "/", "mode": "production", "command": "build" },
        "env": { "command": "build", "mode": "production" },
        "environment": { "name": "client", "mode": "build" },
        "pluginsFormat": "vite",
        "ojStartMode": true,
        "ojBundleHost": true,
    })
    .to_string();
    let host = oj_server::plugins::PluginHost::spawn(
        &app.root,
        &app.root.join("vite.config.mjs"),
        &payload,
        None,
    )
    .await
    .expect("the bundle host starts");
    let facts = host.start_facts().await;
    host.shutdown();
    let facts = facts.expect("start facts are served");
    assert_eq!(
        facts["cloudflare"]["rolldownPlugins"],
        serde_json::json!([{ "name": "builtin:esm-external-require", "options": { "external": ["b"] } }]),
        "{facts}"
    );
}

/// A host over `config` (a vite.config.mjs body) with oj's `overlay` merged
/// into its config (but for `cliInline`, the command line's values, which
/// ride beside it); returns what the config wrote to `seen.log`.
async fn host_run(name: &str, config: &str, command: &str, overlay: serde_json::Value) -> String {
    let app = App::new(&[
        (
            "package.json",
            &format!(r#"{{"name":"{name}","private":true,"type":"module"}}"#),
        ),
        ("vite.config.mjs", config),
    ]);
    app.link_fixture("vite");
    let mode = if command == "build" {
        "production"
    } else {
        "development"
    };
    let mut inline = serde_json::json!({
        "root": app.root.display().to_string(), "mode": mode, "command": command,
    });
    let mut cli = serde_json::json!({});
    for (k, v) in overlay.as_object().unwrap() {
        if k == "cliInline" {
            cli = v.clone();
        } else {
            inline[k] = v.clone();
        }
    }
    let payload = serde_json::json!({
        "config": inline,
        "env": { "command": command, "mode": mode },
        "environment": { "name": "client", "mode": if command == "build" { "build" } else { "dev" } },
        "pluginsFormat": "vite",
        "cliInline": cli,
    })
    .to_string();
    let host = oj_server::plugins::PluginHost::spawn(
        &app.root,
        &app.root.join("vite.config.mjs"),
        &payload,
        None,
    )
    .await
    .expect("the host starts");
    host.plugin_names().await;
    host.shutdown();
    std::fs::read_to_string(app.root.join("seen.log")).unwrap_or_default()
}

/// Vite merges the command line's inline config (`--port`) over the loaded
/// file before any config hook runs, so the hooks see it, and only what was
/// passed: the file's own `base` here. The host then serves oj's values
/// (its resolved base), and a hook's in-place edit of a command-line value
/// stands, as it lands after the merge.
#[tokio::test(flavor = "multi_thread")]
async fn config_hooks_see_the_command_lines_values() {
    require_fixture!("vite");
    let seen = host_run(
        "cli-inline",
        r#"import { appendFileSync } from "node:fs";
import path from "node:path";
const log = (s) => appendFileSync(path.join(__dirname, "seen.log"), s + "\n");
export default {
  base: "/file/",
  server: { port: 1111 },
  plugins: [{
    name: "reads-cli",
    config(config) {
      log(`config ${config.base} ${config.server.port} ${config.server.host}`);
      config.server.host = "edited";
    },
    configResolved(config) {
      log(`resolved ${config.base} ${config.server.port} ${config.server.host}`);
    },
  }],
};
"#,
        "serve",
        serde_json::json!({
            "base": "/from-oj/",
            "server": { "port": 4321, "strictPort": false, "host": "0.0.0.0" },
            "cliInline": { "server": { "port": 4321, "host": "0.0.0.0" } },
        }),
    )
    .await;
    assert_eq!(
        seen,
        "config /file/ 4321 0.0.0.0\nresolved /from-oj/ 4321 edited\n"
    );
}

/// oj's base is the config's resolved one, every config hook applied: a hook
/// that builds on the base it is given sees the file's, not that result, so
/// the host lands on the base oj serves.
#[tokio::test(flavor = "multi_thread")]
async fn a_base_building_hook_is_applied_once() {
    require_fixture!("vite");
    let seen = host_run(
        "base-once",
        r#"import { appendFileSync } from "node:fs";
import path from "node:path";
export default {
  plugins: [{
    name: "sub-base",
    config(config) {
      return { base: (config.base ?? "/") + "sub/" };
    },
    configResolved(config) {
      appendFileSync(path.join(__dirname, "seen.log"), config.base + "\n");
    },
  }],
};
"#,
        "serve",
        serde_json::json!({ "base": "/sub/" }),
    )
    .await;
    assert_eq!(seen, "/sub/\n");
}

/// A config hook that wraps a sibling's configResolved wraps the real one:
/// the host runs the wrapper, which reaches the original, once each.
#[tokio::test(flavor = "multi_thread")]
async fn a_hook_a_sibling_wraps_during_the_resolve_keeps_its_original() {
    require_fixture!("vite");
    let seen = host_run(
        "wrap-sibling",
        r#"import { appendFileSync } from "node:fs";
import path from "node:path";
const log = (s) => appendFileSync(path.join(__dirname, "seen.log"), s + "\n");
const inner = { name: "inner", configResolved() { log("inner"); } };
const wrapper = {
  name: "wrapper",
  config() {
    const original = inner.configResolved;
    inner.configResolved = function (config) {
      log("wrapped");
      return original.call(this, config);
    };
  },
};
export default { plugins: [wrapper, inner] };
"#,
        "serve",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(seen, "wrapped\ninner\n");
}

/// Vite's configEnv takes `isSsrBuild` from the inline config alone (the
/// CLI's `--ssr`), never from the file's `build.ssr`, which the hooks still
/// find in the config.
#[tokio::test(flavor = "multi_thread")]
async fn a_files_build_ssr_is_not_an_ssr_build_for_the_config_hooks() {
    require_fixture!("vite");
    let seen = host_run(
        "file-ssr",
        r#"import { appendFileSync } from "node:fs";
import path from "node:path";
export default {
  build: { ssr: "src/entry.js" },
  plugins: [{
    name: "reads-env",
    config(config, env) {
      appendFileSync(path.join(__dirname, "seen.log"), `${env.isSsrBuild} ${config.build.ssr}\n`);
    },
  }],
};
"#,
        "build",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(seen, "false src/entry.js\n");
}

/// A config hook that gives its own plugin a configResolved while Vite's
/// resolve runs keeps it: the host runs it once, on the host's resolved
/// config (whose plugin list is the host's, not Vite's internal one).
#[tokio::test(flavor = "multi_thread")]
async fn a_hook_a_plugin_sets_during_the_resolve_stays_set() {
    require_fixture!("vite");
    let seen = host_run(
        "lazy-hook",
        r#"import { appendFileSync } from "node:fs";
import path from "node:path";
const plugin = {
  name: "lazy",
  config() {
    plugin.configResolved = (config) =>
      appendFileSync(
        path.join(__dirname, "seen.log"),
        `lazy configResolved, run inside Vite: ${config.plugins.some((p) => p.name === "vite:optimized-deps")}\n`,
      );
  },
};
export default { plugins: [plugin] };
"#,
        "serve",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(seen, "lazy configResolved, run inside Vite: false\n");
}
