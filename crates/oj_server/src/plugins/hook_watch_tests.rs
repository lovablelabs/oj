// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A hook that runs long: the call waiting on it says which module and
//! plugin it waits on, identical concurrent calls share one run (Vite's
//! pending request per url), and a slow module holds up only itself.

use super::host::{long_hook_notice, running_hooks_of, RunningHook, SpawnTimeouts};
use super::*;

fn plugins_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("oj-hook-watch-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

async fn host_with(root: &Path, plugins: &str) -> std::sync::Arc<PluginHost> {
    let file = root.join("oj.plugins.mjs");
    std::fs::write(&file, plugins).unwrap();
    let config = serde_json::json!({
        "config": { "root": root.display().to_string() },
        "env": { "command": "serve", "mode": "development" },
    })
    .to_string();
    PluginHost::spawn_with_timeouts(
        root,
        &file,
        &config,
        SpawnTimeouts {
            probe: Some(std::time::Duration::from_millis(300)),
            ..Default::default()
        },
    )
    .await
    .expect("the embedded engine spawns")
}

// A transform that sleeps for `slow.md` and answers at once for anything
// else, appending each call to the record file.
fn slow_plugin(record: &Path, ms: u64) -> String {
    format!(
        r#"import {{ appendFileSync }} from "node:fs";
export default [{{
  name: "slow-md",
  async transform(code, id) {{
    appendFileSync({record:?}, id.slice(id.lastIndexOf("/") + 1) + "\n");
    if (id.endsWith("/slow.md")) await new Promise((r) => setTimeout(r, {ms}));
    return code + " via slow-md";
  }},
}}];
"#
    )
}

fn calls(record: &Path) -> Vec<String> {
    std::fs::read_to_string(record)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn the_notice_names_the_module_its_phase_and_the_plugin() {
    let running = vec![RunningHook {
        hook: "transformPhase".into(),
        id: Some("/app/src/styles.css".into()),
        phase: Some("normal".into()),
        plugin: Some("@tailwindcss/vite:generate:serve".into()),
        ms: 20_400,
    }];
    assert_eq!(
        long_hook_notice(
            "transformPhase",
            &["body{}", "/app/src/styles.css", "{}", "normal"],
            20,
            &running
        ),
        "oj: transformPhase (normal) of /app/src/styles.css has been running for 20s, \
         in plugin @tailwindcss/vite:generate:serve's transform (the plugin host is responsive)…"
    );
    // Between two handlers the host knows the module but no plugin.
    assert_eq!(
        long_hook_notice("load", &["/app/src/a.ts"], 40, &[]),
        "oj: load of /app/src/a.ts has been running for 40s (the plugin host is responsive)…"
    );
    // Another hook's entry never names this call's plugin.
    assert_eq!(
        long_hook_notice("transform", &["x", "/app/other.ts", "{}"], 20, &running),
        "oj: transform of /app/other.ts has been running for 20s (the plugin host is responsive)…"
    );
    // A lifecycle hook has no module.
    assert_eq!(
        long_hook_notice("buildStart", &[], 60, &[]),
        "oj: buildStart has been running for 60s (the plugin host is responsive)…"
    );
    // An older host's `true` is no list.
    assert!(running_hooks_of(&serde_json::Value::Bool(true)).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_running_hook_reports_the_plugin_it_awaits() {
    let root = plugins_root("running");
    let record = root.join("calls.txt");
    let host = host_with(&root, &slow_plugin(&record, 1500)).await;
    host.resolve_id("x", "").await.expect("a live host serves");
    let slow = root.join("slow.md").display().to_string();
    let call = {
        let host = std::sync::Arc::clone(&host);
        let slow = slow.clone();
        tokio::spawn(async move { host.transform("# s", &slow, "").await })
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let seen = loop {
        let running = host.running_hooks().await;
        if let Some(r) = running.iter().find(|r| r.plugin.is_some()) {
            break r.clone();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the running transform never showed up: {running:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    };
    assert_eq!(seen.hook, "transform");
    assert_eq!(seen.id.as_deref(), Some(slow.as_str()));
    assert_eq!(seen.plugin.as_deref(), Some("slow-md"));
    assert_eq!(seen.phase, None);
    assert_eq!(
        call.await.unwrap().unwrap().code,
        "# s via slow-md",
        "the hook still answers"
    );
    assert!(
        !host
            .running_hooks()
            .await
            .iter()
            .any(|r| r.hook == "transform"),
        "a finished hook is no longer running"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn identical_concurrent_module_hook_calls_share_one_run() {
    let root = plugins_root("shared");
    let record = root.join("calls.txt");
    let host = host_with(&root, &slow_plugin(&record, 600)).await;
    let slow = root.join("slow.md").display().to_string();
    let fast = root.join("fast.md").display().to_string();
    // Four pages asking for the same module at once: one run of its plugins.
    let same = (0..4).map(|_| host.transform("# s", &slow, ""));
    let results = futures_util::future::join_all(same).await;
    for r in results {
        assert_eq!(r.unwrap().code, "# s via slow-md");
    }
    assert_eq!(calls(&record), ["slow.md"], "the plugin ran once");
    // Another module, or the same one asked again later, is its own run:
    // this is Vite's pending request, not a cache.
    host.transform("# f", &fast, "").await.unwrap();
    host.transform("# s", &slow, "").await.unwrap();
    assert_eq!(calls(&record), ["slow.md", "fast.md", "slow.md"]);
    // A different code for the same id is a different request too.
    host.transform("# s2", &slow, "").await.unwrap();
    assert_eq!(calls(&record).len(), 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slow_hook_holds_up_only_its_own_module() {
    let root = plugins_root("own");
    let record = root.join("calls.txt");
    let host = host_with(&root, &slow_plugin(&record, 1500)).await;
    let slow = root.join("slow.md").display().to_string();
    let fast = root.join("fast.md").display().to_string();
    let pending = {
        let host = std::sync::Arc::clone(&host);
        tokio::spawn(async move { host.transform("# s", &slow, "").await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let started = std::time::Instant::now();
    let quick = host.transform("# f", &fast, "").await.unwrap();
    assert_eq!(quick.code, "# f via slow-md");
    assert!(
        started.elapsed() < std::time::Duration::from_millis(1000),
        "another module waited on the slow one: {:?}",
        started.elapsed()
    );
    pending.await.unwrap().unwrap();
}
