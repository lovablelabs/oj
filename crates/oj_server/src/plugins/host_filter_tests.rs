// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A Start host leaves out the framework plugins oj reimplements (`vite:*`,
//! TanStack's), whatever shape the config hands them in. Vite reads
//! `plugins` through asyncFlatten (config.ts resolveConfig, plugin.ts
//! resolveEnvironmentPlugins): a promise may resolve to an array, an array
//! may hold more promises, and every level is flattened before any plugin is
//! looked at. A filter that flattens before it awaits sees
//! `Promise.resolve([tanstackStart()])` as one nameless entry and keeps it.

use super::*;

/// The plugins a config of `plugins_src` leaves in a host spawned with
/// `extra` merged into its init payload. No Vite is installed, so a
/// vite-format config loads by plain import.
async fn hosted_names(
    tag: &str,
    file: &str,
    plugins_src: &str,
    extra: serde_json::Value,
) -> Vec<String> {
    let root = std::env::temp_dir().join(format!("oj-host-filter-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join(file);
    std::fs::write(&path, plugins_src).unwrap();
    let mut payload = serde_json::json!({
        "config": { "root": root.display().to_string(), "command": "build", "mode": "production" },
        "env": { "command": "build", "mode": "production" },
    });
    for (k, v) in extra.as_object().unwrap() {
        payload[k] = v.clone();
    }
    let host = PluginHost::spawn(&root, &path, &payload.to_string(), None)
        .await
        .expect("the host spawns");
    let names = host.plugin_names().await;
    host.shutdown();
    let _ = std::fs::remove_dir_all(&root);
    names
}

const PROMISED_TANSTACK: &str = r#"const tanstackStart = () => [
  { name: "tanstack-start-core:config" },
  Promise.resolve({ name: "tanstack:router-generator" }),
  { name: "vite:react-babel" },
];
export default {
  plugins: [
    { name: "user-first" },
    Promise.resolve(tanstackStart()),
    Promise.resolve(Promise.resolve([[{ name: "tanstack-react-start:server-fn" }], { name: "user-nested" }])),
    null,
    { name: "user-last" },
  ],
};
"#;

fn bundle_host(env_name: &str) -> serde_json::Value {
    serde_json::json!({
        "environment": { "name": env_name, "mode": "build" },
        "pluginsFormat": "vite",
        "ojStartMode": true,
        "ojBundleHost": true,
    })
}

#[tokio::test]
async fn start_bundle_host_drops_promise_wrapped_framework_plugins() {
    let names = hosted_names(
        "bundle",
        "vite.config.mjs",
        PROMISED_TANSTACK,
        bundle_host("client"),
    )
    .await;
    assert_eq!(names, ["user-first", "user-nested", "user-last"]);
}

#[tokio::test]
async fn start_ssr_host_drops_promise_wrapped_framework_plugins() {
    let extra = serde_json::json!({
        "environment": { "name": "ssr", "mode": "dev" },
        "pluginsFormat": "vite",
        "ojStartMode": true,
    });
    let names = hosted_names("ssr", "vite.config.mjs", PROMISED_TANSTACK, extra).await;
    assert_eq!(names, ["user-first", "user-nested", "user-last"]);
}

// Outside Start only oj's native plugins (vite:react-babel) are left out: the
// rest of the same config's plugins run, each flattened to its own entry.
#[tokio::test]
async fn non_start_host_flattens_promised_plugins_into_entries() {
    let extra = serde_json::json!({
        "environment": { "name": "client", "mode": "build" },
        "pluginsFormat": "vite",
    });
    let names = hosted_names("plain", "vite.config.mjs", PROMISED_TANSTACK, extra).await;
    assert_eq!(
        names,
        [
            "user-first",
            "tanstack-start-core:config",
            "tanstack:router-generator",
            "tanstack-react-start:server-fn",
            "user-nested",
            "user-last",
        ]
    );
}

// An oj plugins file's default export may be a promise of the list too.
#[tokio::test]
async fn oj_plugins_file_export_is_flattened_like_vite_plugins() {
    let src = r#"export default Promise.resolve([
  { name: "user-a" },
  Promise.resolve([{ name: "tanstack-start-core:config" }, { name: "user-b" }]),
]);
"#;
    let extra = serde_json::json!({
        "environment": { "name": "client", "mode": "build" },
        "pluginsFormat": "oj",
        "ojStartMode": true,
        "ojBundleHost": true,
    });
    let names = hosted_names("oj", "oj.plugins.mjs", src, extra).await;
    assert_eq!(names, ["user-a", "user-b"]);
}

// Vite's resolveEnvironmentPlugins: what applyToEnvironment returns is
// asyncFlattened, so a promised array replaces the plugin with its members.
#[tokio::test]
async fn apply_to_environment_promised_array_is_flattened() {
    let src = r#"export default {
  plugins: [{
    name: "per-env",
    applyToEnvironment: (env) =>
      Promise.resolve([{ name: `per-env:${env.name}` }, Promise.resolve([{ name: "per-env:extra" }]), false]),
  }],
};
"#;
    let names = hosted_names("apply-env", "vite.config.mjs", src, bundle_host("client")).await;
    assert_eq!(names, ["per-env:client", "per-env:extra"]);
}

// What applyToEnvironment returns passes the same filter as the top-level
// list: a wrapper handing back TanStack's plugins does not smuggle them in.
#[tokio::test]
async fn apply_to_environment_cannot_return_framework_plugins_into_a_start_host() {
    let src = r#"export default {
  plugins: [{
    name: "wrap",
    applyToEnvironment: () =>
      Promise.resolve([{ name: "tanstack-start-core:config" }, { name: "vite:react-babel" }, { name: "wrapped-user" }]),
  }],
};
"#;
    let names = hosted_names(
        "apply-env-start",
        "vite.config.mjs",
        src,
        bundle_host("client"),
    )
    .await;
    assert_eq!(names, ["wrapped-user"]);
}

// A disabled factory may null its plugin's name: the Start filter must read
// it as no name, not throw and leave the host with no plugins at all.
#[tokio::test]
async fn a_null_plugin_name_does_not_empty_a_start_host() {
    let src = r#"export default {
  plugins: [
    { name: null, transform() {} },
    { name: "user" },
    { name: "wrap", applyToEnvironment: () => [{ name: null }, { name: "wrapped" }] },
  ],
};
"#;
    let names = hosted_names("null-name", "vite.config.mjs", src, bundle_host("client")).await;
    assert!(
        names.iter().any(|n| n == "user") && names.iter().any(|n| n == "wrapped"),
        "the host's plugins: {names:?}"
    );
}

// Promise.all treats an object whose `then` is no function as a value, so
// the flatten must too, or it would await the same list forever.
#[tokio::test]
async fn a_plugin_with_a_non_function_then_does_not_hang_the_host() {
    let src = r#"export default {
  plugins: [{ name: "thenful", then: true }, Promise.resolve({ name: "after" })],
};
"#;
    let extra = serde_json::json!({
        "environment": { "name": "client", "mode": "build" },
        "pluginsFormat": "vite",
    });
    let names = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        hosted_names("thenful", "vite.config.mjs", src, extra),
    )
    .await
    .expect("the host finished loading its plugins");
    assert_eq!(names, ["thenful", "after"]);
}

// A rejection inside what applyToEnvironment returned is the hook failing,
// like a throw from the hook itself: Start mode (which tolerates a framework
// config's lifecycle hooks per plugin) keeps the plugin active and the host
// comes up; elsewhere it fails the boot, as it rejects Vite's createServer.
#[tokio::test]
async fn in_start_mode_a_rejection_inside_an_apply_to_environment_result_keeps_the_plugin() {
    let src = r#"export default {
  plugins: [
    { name: "user" },
    { name: "rejects", applyToEnvironment: () => [Promise.reject(new Error("nested"))] },
  ],
};
"#;
    let extra = serde_json::json!({
        "environment": { "name": "client", "mode": "build" },
        "pluginsFormat": "vite",
        "ojStartMode": true,
    });
    let names = hosted_names("nested-reject", "vite.config.mjs", src, extra).await;
    assert_eq!(names, ["user", "rejects"]);
}
