// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A hook that runs long: the call waiting on it says which module and
//! plugin it waits on (through a handler's own `this.load` too), identical
//! concurrent calls share one run (Vite's pending request per url), and a
//! slow module holds up only itself.

use super::host::{long_hook_notice, running_hooks_of, RunningHook, SpawnTimeouts};
use super::*;

fn plugins_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("oj-hook-watch-{name}-{}", std::process::id()));
    crate::plugins::test_roots::remove_root(&root);
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
    // A different code for the same id is a different request too, also
    // while the other is still running (two runs under one module).
    let (a, b) = tokio::join!(
        host.transform("# a", &slow, ""),
        host.transform("# b", &slow, "")
    );
    assert_eq!(a.unwrap().code, "# a via slow-md");
    assert_eq!(b.unwrap().code, "# b via slow-md");
    assert_eq!(calls(&record).len(), 5);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slow_hook_holds_up_only_its_own_module() {
    let root = plugins_root("own");
    let record = root.join("calls.txt");
    let host = host_with(&root, &slow_plugin(&record, 1500)).await;
    host.resolve_id("x", "").await.expect("a live host serves");
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

// A handler that runs the chain again itself (`this.load`, as a router
// plugin walking a directory or a stylesheet plugin scanning sources does)
// is still the plugin the hook awaits once that inner chain settles.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_handler_loading_another_module_stays_the_named_plugin() {
    let root = plugins_root("nested");
    let virt = root.join("virt.md").display().to_string();
    let plugins = format!(
        r#"export default [{{
  name: "outer",
  async transform(code, id) {{
    if (id.endsWith("/slow.md")) {{
      await this.load({{ id: {virt:?} }});
      await new Promise((r) => setTimeout(r, 1500));
    }}
    return code;
  }},
}}, {{
  name: "inner",
  load(id) {{ if (id.endsWith("/virt.md")) return "x"; }},
}}];
"#
    );
    let host = host_with(&root, &plugins).await;
    host.resolve_id("x", "").await.expect("a live host serves");
    let slow = root.join("slow.md").display().to_string();
    let call = {
        let host = std::sync::Arc::clone(&host);
        let slow = slow.clone();
        tokio::spawn(async move { host.transform("# s", &slow, "").await })
    };
    // Past the inner load (immediate) and into the outer handler's sleep.
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    let running = host.running_hooks().await;
    let outer = running
        .iter()
        .find(|r| r.hook == "transform" && r.id.as_deref() == Some(slow.as_str()))
        .unwrap_or_else(|| panic!("the outer transform is not running: {running:?}"));
    assert_eq!(
        outer.plugin.as_deref(),
        Some("outer"),
        "the inner chain un-named the handler still awaiting: {running:?}"
    );
    assert_eq!(call.await.unwrap().unwrap().code, "# s");
}

// A call made after a change never shares a run begun before it: the run
// read the files as they were, and a virtual module's `load` is keyed by
// its id alone. A call made before the change still shares it (Vite's
// `transformRequest` keeps a pending request only while its timestamp is
// past the module's last invalidation).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_call_after_a_change_never_shares_a_run_begun_before_it() {
    let root = plugins_root("changed");
    let record = root.join("calls.txt");
    let plugins = format!(
        r#"import {{ appendFileSync }} from "node:fs";
export default [{{
  name: "virt",
  async load(id) {{
    if (!id.endsWith("/virt.md")) return null;
    appendFileSync({record:?}, "load\n");
    await new Promise((r) => setTimeout(r, 600));
    return "export default 'v'";
  }},
}}];
"#
    );
    let host = host_with(&root, &plugins).await;
    host.resolve_id("x", "").await.expect("a live host serves");
    let virt = root.join("virt.md").display().to_string();
    let first = {
        let host = std::sync::Arc::clone(&host);
        let virt = virt.clone();
        tokio::spawn(async move { host.load(&virt).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    // Before any change: the same call, so the same run. The task has to
    // reach the host before the change below, or it would be a call after it.
    let before = {
        let host = std::sync::Arc::clone(&host);
        let virt = virt.clone();
        tokio::spawn(async move { host.load(&virt).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    // A watched file changed: what the running load read may be stale.
    host.watch_change(&root.join("notes.md").display().to_string(), "update")
        .await
        .unwrap();
    let after = host.load(&virt).await.unwrap();
    assert_eq!(after.as_deref(), Some("export default 'v'"));
    assert_eq!(
        first.await.unwrap().unwrap().as_deref(),
        Some("export default 'v'")
    );
    assert_eq!(
        before.await.unwrap().unwrap().as_deref(),
        Some("export default 'v'")
    );
    assert_eq!(
        calls(&record),
        ["load", "load"],
        "the two calls before the change shared one run; the one after ran again"
    );
}
