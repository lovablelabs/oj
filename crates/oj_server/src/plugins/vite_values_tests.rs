use super::*;

#[test]
fn finds_commonjs_vite_config_formats() {
    for extension in ["cjs", "cts"] {
        let root = std::env::temp_dir().join(format!(
            "oj-config-format-{}-{extension}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join(format!("vite.config.{extension}"));
        std::fs::write(&path, "module.exports = {};").unwrap();
        assert_eq!(vite_config_file(&root, None), Some(path));
        std::fs::remove_dir_all(&root).unwrap();
    }
}

#[test]
fn plugin_rpc_timeout_defaults_and_reads_env_seconds() {
    assert_eq!(plugin_rpc_timeout_from(None).as_secs(), 20);
    assert_eq!(plugin_rpc_timeout_from(Some("90")).as_secs(), 90);
    assert_eq!(plugin_rpc_timeout_from(Some(" 5 ")).as_secs(), 5);
    // Garbage and zero fall back to the default rather than disabling the guard.
    assert_eq!(plugin_rpc_timeout_from(Some("soon")).as_secs(), 20);
    assert_eq!(plugin_rpc_timeout_from(Some("0")).as_secs(), 20);
}

#[test]
fn plugin_init_timeout_defaults_and_reads_env_seconds() {
    assert_eq!(plugin_init_timeout_from(None).as_secs(), 300);
    assert_eq!(plugin_init_timeout_from(Some("2")).as_secs(), 2);
    assert_eq!(plugin_init_timeout_from(Some("soon")).as_secs(), 300);
    assert_eq!(plugin_init_timeout_from(Some("0")).as_secs(), 300);
}

#[test]
fn extraction_timeout_defaults_and_reads_env_seconds() {
    assert_eq!(extraction_timeout_from(None).as_secs(), 60);
    assert_eq!(extraction_timeout_from(Some("120")).as_secs(), 120);
    assert_eq!(extraction_timeout_from(Some("junk")).as_secs(), 60);
    assert_eq!(extraction_timeout_from(Some("0")).as_secs(), 60);
}

// Init-wait policy: a boot host waits out the long init deadline, a lazy one
// only the short per-call bound.
#[test]
fn init_wait_policy_is_long_for_boot_hosts_and_short_for_lazy_ones() {
    let (boot_wait, boot_knob) = init_wait_policy(false);
    assert_eq!(boot_wait, plugin_init_timeout());
    assert_eq!(boot_knob, "OJ_PLUGIN_INIT_TIMEOUT");
    let (lazy_wait, lazy_knob) = init_wait_policy(true);
    assert_eq!(lazy_wait, plugin_rpc_timeout());
    assert_eq!(lazy_knob, "OJ_PLUGIN_TIMEOUT");
}

// A lazy host's init gate is per-call: a call past spawn + init_wait gets its
// own full window; a boot host keeps the shared spawn-anchored deadline.
#[test]
fn lazy_call_past_the_spawn_deadline_gets_its_own_init_window() {
    let wait = std::time::Duration::from_secs(20);
    let spawned = tokio::time::Instant::now();
    // A call 40 s after spawn, with the 20 s window long since elapsed.
    let now = spawned + std::time::Duration::from_secs(40);
    let lazy = call_init_deadline(true, spawned, wait, now);
    assert_eq!(
        lazy,
        now + wait,
        "the lazy window anchors to the call's own start"
    );
    let boot = call_init_deadline(false, spawned, wait, now);
    assert_eq!(
        boot,
        spawned + wait,
        "the boot deadline stays shared and spawn-anchored"
    );
    assert!(
        boot <= now,
        "sanity: the boot deadline has elapsed for this call"
    );
}

// Only the FIRST report per generation declares the host gone; duplicates and
// stale generations are dropped, a revived generation reports fresh.
#[test]
fn only_the_first_death_report_per_generation_declares_the_host_gone() {
    let mut revive = ReviveState {
        generation: 3,
        attempts: 0,
        last: None,
        pending_before: std::collections::HashSet::new(),
        reported: false,
    };
    assert!(first_death_report(&mut revive, 3), "the first report wins");
    assert!(
        !first_death_report(&mut revive, 3),
        "a concurrent belt's duplicate is dropped"
    );
    assert!(
        !first_death_report(&mut revive, 2),
        "a stale generation's report is dropped"
    );
    // A revive bumps the generation and re-arms reporting (try_revive).
    revive.generation += 1;
    revive.reported = false;
    assert!(
        !first_death_report(&mut revive, 3),
        "the dead generation stays declared"
    );
    assert!(
        first_death_report(&mut revive, 4),
        "the fresh generation reports its own death"
    );
    assert!(!first_death_report(&mut revive, 4), "once");
}

// An oj-side ssr key must not drop the extractor's verdict: per-key merge,
// `runnerBacked` always adopted.
#[test]
fn merge_fills_ssr_per_key_and_always_adopts_runner_backed() {
    let mut config = oj_config::OjConfig {
        ssr: Some(serde_json::json!({ "noExternal": true })),
        ..Default::default()
    };
    let v = ViteValues {
        ssr: Some(serde_json::json!({
            "noExternal": ["from-vite"],
            "target": "webworker",
            "runnerBacked": true,
            "resolve": { "conditions": ["workerd"] }
        })),
        ..Default::default()
    };
    merge_vite_values(&mut config, v);
    let ssr = config.ssr.as_ref().unwrap();
    assert_eq!(
        ssr["noExternal"],
        serde_json::json!(true),
        "the oj config's key wins"
    );
    assert_eq!(
        ssr["target"], "webworker",
        "extractor keys fill where oj lacks them"
    );
    assert_eq!(ssr["resolve"]["conditions"][0], "workerd");
    assert!(
        oj_config::ssr_runner_backed(&config),
        "the verdict survives an oj-side ssr key"
    );

    // runnerBacked is always the extractor's, even against a (stale)
    // oj-side value: only extraction produces it.
    let mut config = oj_config::OjConfig {
        ssr: Some(serde_json::json!({ "runnerBacked": false })),
        ..Default::default()
    };
    let v = ViteValues {
        ssr: Some(serde_json::json!({ "runnerBacked": true })),
        ..Default::default()
    };
    merge_vite_values(&mut config, v);
    assert!(oj_config::ssr_runner_backed(&config));
}

// The ssr merge recurses one level into `resolve`: oj-side sub-keys must not
// drop the extractor's others (the workerd sugar's `conditions` above all).
#[test]
fn merge_recurses_one_level_into_ssr_resolve() {
    let mut config = oj_config::OjConfig {
        ssr: Some(serde_json::json!({ "resolve": { "externalConditions": ["oj-ext"] } })),
        ..Default::default()
    };
    let v = ViteValues {
        ssr: Some(serde_json::json!({
            "runnerBacked": true,
            "resolve": { "conditions": ["workerd"], "externalConditions": ["never-adopted"] }
        })),
        ..Default::default()
    };
    merge_vite_values(&mut config, v);
    let ssr = config.ssr.as_ref().unwrap();
    assert_eq!(
        ssr["resolve"]["externalConditions"],
        serde_json::json!(["oj-ext"]),
        "the oj config's sub-key wins"
    );
    assert_eq!(
        ssr["resolve"]["conditions"],
        serde_json::json!(["workerd"]),
        "the extractor's other resolve sub-keys fill in"
    );
    assert!(oj_config::ssr_runner_backed(&config));
}

// A non-object oj-side ssr (or ssr.resolve) cannot merge per-key: the
// extractor block is adopted so the runnerBacked contract holds.
#[test]
fn merge_adopts_extractor_ssr_when_the_oj_side_is_not_an_object() {
    let mut config = oj_config::OjConfig {
        ssr: Some(serde_json::json!("bogus")),
        ..Default::default()
    };
    let v = ViteValues {
        ssr: Some(serde_json::json!({ "runnerBacked": true, "target": "webworker" })),
        ..Default::default()
    };
    merge_vite_values(&mut config, v);
    assert!(
        oj_config::ssr_runner_backed(&config),
        "the contract holds against a non-object oj-side ssr"
    );
    assert_eq!(config.ssr.as_ref().unwrap()["target"], "webworker");

    // Same one level down: a non-object ssr.resolve adopts the
    // extractor's resolve block instead of silently dropping the sugar.
    let mut config = oj_config::OjConfig {
        ssr: Some(serde_json::json!({ "resolve": "bogus" })),
        ..Default::default()
    };
    let v = ViteValues {
        ssr: Some(serde_json::json!({
            "runnerBacked": true,
            "resolve": { "conditions": ["workerd"] }
        })),
        ..Default::default()
    };
    merge_vite_values(&mut config, v);
    let ssr = config.ssr.as_ref().unwrap();
    assert_eq!(ssr["resolve"]["conditions"], serde_json::json!(["workerd"]));
    assert!(oj_config::ssr_runner_backed(&config));
}

/// Spawn a healthy lazy host over a trivial plugins file and prove it serves
/// a call: the shared setup of the revive tests.
async fn spawn_live_host(tag: &str) -> (PathBuf, std::sync::Arc<PluginHost>) {
    let root = std::env::temp_dir().join(format!("oj-revive-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let plugins = root.join("oj.plugins.mjs");
    std::fs::write(&plugins, "export default [];\n").unwrap();
    let config = serde_json::json!({
        "config": { "root": root.display().to_string() },
        "env": { "command": "serve", "mode": "development" },
    })
    .to_string();
    let host = PluginHost::spawn_lazy_with_wait(
        &root,
        &plugins,
        &config,
        std::time::Duration::from_secs(30),
    )
    .await
    .expect("the embedded engine spawns");
    host.resolve_id("x", "").await.expect("a live host serves");
    (root, host)
}

/// Rewind respawn spacing after an induced death: these tests target revive
/// semantics, not keeper timing.
fn clear_respawn_spacing(host: &PluginHost) {
    host.revive.lock().unwrap().last =
        Some(std::time::Instant::now() - PLUGIN_HOST_RESPAWN_SPACING);
}

// A wedge is no longer terminal: the next call revives the host with a
// fresh engine generation through the same boot path, and serves.
#[tokio::test]
async fn a_gone_host_is_revived_by_the_next_call() {
    let (_root, host) = spawn_live_host("basic").await;
    let generation = host.revive.lock().unwrap().generation;
    host.declare_gone("test wedge", generation);
    clear_respawn_spacing(&host);
    assert!(*host.host_gone.borrow(), "the death latched");

    host.resolve_id("x", "")
        .await
        .expect("the next call revives the host and serves");
    let revive = host.revive.lock().unwrap();
    assert_eq!(
        revive.generation,
        generation + 1,
        "a fresh engine generation"
    );
    assert_eq!(revive.attempts, 1, "one respawn consumed");
    drop(revive);
    assert!(!*host.host_gone.borrow(), "the host is live again");
}

// A death report about a replaced engine (an old call's transport belt
// firing after a revive) is stale and must not kill the new generation.
#[tokio::test]
async fn a_stale_death_report_does_not_kill_a_revived_host() {
    let (_root, host) = spawn_live_host("stale").await;
    let generation = host.revive.lock().unwrap().generation;
    host.declare_gone("test wedge", generation);
    clear_respawn_spacing(&host);
    host.resolve_id("x", "").await.expect("revived");

    host.declare_gone("stale report about the old engine", generation);
    assert!(
        !*host.host_gone.borrow(),
        "a stale-generation report is ignored"
    );
    host.resolve_id("x", "").await.expect("still serving");
}

// shutdown() retires the host on purpose: never revived.
#[tokio::test]
async fn a_shutdown_host_is_never_revived() {
    let (_root, host) = spawn_live_host("shutdown").await;
    host.shutdown();
    host.host_gone_wait().await;
    let err = host.resolve_id("x", "").await.expect_err("stays dead");
    assert!(err.contains("plugin host exited"), "{err}");
    assert_eq!(host.revive.lock().unwrap().attempts, 0, "no respawn burned");
}

// Keeper is best effort per addon: an unloadable path is skipped, the eval
// succeeds, the keeper engine stays up for the next death.
#[tokio::test]
async fn addon_keeper_tolerates_unloadable_addons() {
    let root = std::env::temp_dir().join(format!("oj-keeper-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    keep_addons_alive(
        &root,
        &[
            PathBuf::from("/nonexistent/fake-binding.node"),
            PathBuf::from("/also/missing.node"),
        ],
        None,
    )
    .await
    .expect("the keeper load is best effort");
    assert!(
        ADDON_KEEPER.lock().unwrap().is_some(),
        "the keeper engine stays resident"
    );
}

// Heap cap mirrors NODE_OPTIONS --max-old-space-size (last wins, underscore
// accepted, like Node), OJ_PLUGIN_MEMORY_MB above it, 4096 beneath.
#[test]
fn node_options_heap_cap_parses_like_node() {
    assert_eq!(max_old_space_mb("--max-old-space-size=8192"), Some(8192));
    assert_eq!(
        max_old_space_mb("--dns-result-order=ipv4first --max-old-space-size=3072 --expose-gc"),
        Some(3072)
    );
    assert_eq!(max_old_space_mb("--max_old_space_size=2048"), Some(2048));
    assert_eq!(
        max_old_space_mb("--max-old-space-size=1024 --max-old-space-size=512"),
        Some(512)
    );
    assert_eq!(max_old_space_mb("--max-old-space-size=zero"), None);
    assert_eq!(max_old_space_mb("--max-semi-space-size=64"), None);
    assert_eq!(max_old_space_mb(""), None);
}

// A heap blow-up fails the running hook with MemoryLimit, declares the host
// gone, and the next call revives it on a fresh heap.
#[tokio::test]
async fn a_memory_blowup_declares_the_host_gone_and_the_next_call_revives_it() {
    let root = std::env::temp_dir().join(format!("oj-revive-oom-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let plugins = root.join("oj.plugins.mjs");
    std::fs::write(
        &plugins,
        r#"const hog = [];
export default [{
  name: "heap-hog",
  load(id) {
if (id.includes("__hog__")) {
  for (;;) hog.push(new Array(1024 * 1024).fill(Math.random()));
}
return null;
  },
}];
"#,
    )
    .unwrap();
    let config = serde_json::json!({
        "config": { "root": root.display().to_string() },
        "env": { "command": "serve", "mode": "development" },
    })
    .to_string();
    let host = PluginHost::spawn_with_timeouts(
        &root,
        &plugins,
        &config,
        true,
        SpawnTimeouts {
            init_wait: Some(std::time::Duration::from_secs(30)),
            rpc: Some(std::time::Duration::from_secs(20)),
            memory: Some(128 * 1024 * 1024),
            ..Default::default()
        },
    )
    .await
    .expect("the embedded engine spawns");
    host.load("warmup").await.expect("healthy host answers");

    let err = host
        .load("__hog__")
        .await
        .expect_err("the heap cap fails the allocating hook");
    assert!(
        err.contains("memory limit") && err.contains("OJ_PLUGIN_MEMORY_MB"),
        "the failure names the cap and its knob: {err}"
    );
    assert!(*host.host_gone.borrow(), "a blown heap retires the engine");

    clear_respawn_spacing(&host);
    host.load("after")
        .await
        .expect("the next call revives the host on a fresh heap");
    assert_eq!(
        host.revive.lock().unwrap().attempts,
        1,
        "one respawn consumed"
    );
}

// Budget is a LIFETIME cap: past it the host stays gone, and attempts are
// spaced so a burst cannot stack engines.
#[tokio::test]
async fn the_respawn_budget_is_finite_and_spaced() {
    let (_root, host) = spawn_live_host("budget").await;
    for round in 0..PLUGIN_HOST_RESPAWN_LIMIT {
        let generation = host.revive.lock().unwrap().generation;
        host.declare_gone("recurring test wedge", generation);
        if round == 0 {
            clear_respawn_spacing(&host);
        }
        // Immediately after a previous revive the spacing rejects the
        // attempt; backdate the clock instead of sleeping it out.
        if round > 0 {
            let err = host.resolve_id("x", "").await.expect_err("spacing rejects");
            assert!(err.contains("plugin host exited"), "{err}");
            host.revive.lock().unwrap().last =
                Some(std::time::Instant::now() - PLUGIN_HOST_RESPAWN_SPACING);
        }
        host.resolve_id("x", "")
            .await
            .expect("revives within budget");
    }
    let generation = host.revive.lock().unwrap().generation;
    host.declare_gone("one wedge too many", generation);
    host.revive.lock().unwrap().last =
        Some(std::time::Instant::now() - PLUGIN_HOST_RESPAWN_SPACING);
    let err = host.resolve_id("x", "").await.expect_err("budget spent");
    assert!(err.contains("plugin host exited"), "{err}");
    assert!(!host.can_revive(), "no revive left for waiters to hold on");
}

#[tokio::test]
async fn pre_init_calls_keep_their_own_window_after_an_earlier_one_expired() {
    let root = std::env::temp_dir().join(format!("oj-lazy-window-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    // A plugins file whose top-level init is slow but healthy.
    let plugins = root.join("oj.plugins.mjs");
    std::fs::write(
        &plugins,
        "await new Promise((r) => setTimeout(r, 2500));\nexport default [];\n",
    )
    .unwrap();
    let config = serde_json::json!({
        "config": { "root": root.display().to_string() },
        "env": { "command": "serve", "mode": "development" },
    })
    .to_string();
    let host = match PluginHost::spawn_lazy_with_wait(
        &root,
        &plugins,
        &config,
        std::time::Duration::from_secs(1),
    )
    .await
    {
        Ok(h) => h,
        Err(e) => panic!("the embedded engine spawns: {e}"),
    };
    let mut evidence = host.init_failure_updates();
    assert!(
        !*evidence.borrow_and_update(),
        "no evidence before a window expires"
    );

    // First call waits its full per-call window, then fails on the window,
    // flipping the evidence watch.
    let t0 = std::time::Instant::now();
    let first = host.resolve_id("x", "").await;
    let first_err = first.expect_err("init outlives the first call's window");
    assert!(first_err.contains("still initializing"), "{first_err}");
    assert!(
        t0.elapsed() >= std::time::Duration::from_millis(900),
        "the first call waits its full window, got {:?}",
        t0.elapsed()
    );
    assert!(
        *evidence.borrow_and_update(),
        "the expired window is wedge evidence"
    );

    // Later calls each keep their OWN full window (never the removed
    // fail-fast latch), so one is served the moment init lands.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let t = std::time::Instant::now();
        match host.resolve_id("x", "").await {
            Ok(_) => break,
            Err(e) => {
                assert!(
                    e.contains("still initializing"),
                    "never a latched fail-fast: {e}"
                );
                assert!(
                    t.elapsed() >= std::time::Duration::from_millis(900),
                    "a pre-init call after an expired window still gets its own window, got {:?}",
                    t.elapsed()
                );
                assert!(
                    std::time::Instant::now() < deadline,
                    "a late init never served a waiting call: {e}"
                );
            }
        }
    }
    assert!(
        !*evidence.borrow_and_update(),
        "init progressing clears the wedge evidence"
    );
}

// Submit-before-gate hazard, pinned: pre-init calls must submit NOTHING;
// concurrent calls each fail at their own window, proving none queued on the wedged engine.
#[tokio::test]
async fn wedged_host_pre_init_calls_fail_at_their_window_without_submitting_jobs() {
    let root = std::env::temp_dir().join(format!("oj-wedged-stdin-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    // Init never completes; the interval keeps the event loop alive (the
    // shape that wedged the old node child).
    let plugins = root.join("oj.plugins.mjs");
    std::fs::write(
        &plugins,
        "setInterval(() => {}, 1000);\nawait new Promise(() => {});\nexport default [];\n",
    )
    .unwrap();
    let config = serde_json::json!({
        "config": { "root": root.display().to_string() },
        "env": { "command": "serve", "mode": "development" },
    })
    .to_string();
    let host = match PluginHost::spawn_lazy_with_wait(
        &root,
        &plugins,
        &config,
        std::time::Duration::from_secs(1),
    )
    .await
    {
        Ok(h) => h,
        Err(e) => panic!("the embedded engine spawns: {e}"),
    };
    // Far past any OS pipe buffer, the old transport's wedge trigger: the
    // write-first path would have blocked here instead of gating on init.
    let big = "x".repeat(2 * 1024 * 1024);
    let t0 = std::time::Instant::now();
    let (a, b) = tokio::join!(host.resolve_id(&big, ""), host.resolve_id(&big, ""));
    for res in [a, b] {
        let err = res.expect_err("a wedged host fails pre-init calls at their window");
        assert!(err.contains("still initializing"), "{err}");
    }
    let elapsed = t0.elapsed();
    assert!(
        elapsed >= std::time::Duration::from_millis(900),
        "each call waits its window, got {elapsed:?}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "concurrent windows, not calls serialized behind a wedged engine: {elapsed:?}"
    );
}

// Stall monitor is the boot host's real evidence flip site: a wedge flips
// evidence at the stall window with NO call in flight; a merely slow host flips then clears on init.
#[tokio::test]
async fn boot_host_stall_monitor_flips_evidence_without_a_call_and_init_clears_it() {
    let root = std::env::temp_dir().join(format!("oj-boot-stall-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let config = serde_json::json!({
        "config": { "root": root.display().to_string() },
        "env": { "command": "serve", "mode": "development" },
    })
    .to_string();

    // A host that wedges forever during its plugins-module evaluation:
    // after the "start" milestone, no progress ever again.
    let plugins = root.join("oj.plugins.mjs");
    std::fs::write(
        &plugins,
        "setInterval(() => {}, 1000);\nawait new Promise(() => {});\nexport default [];\n",
    )
    .unwrap();
    let host = match PluginHost::spawn_with_timeouts(
        &root,
        &plugins,
        &config,
        false,
        SpawnTimeouts {
            init_wait: Some(std::time::Duration::from_secs(120)),
            stall: Some(std::time::Duration::from_secs(1)),
            ..Default::default()
        },
    )
    .await
    {
        Ok(h) => h,
        Err(e) => panic!("the embedded engine spawns: {e}"),
    };
    let mut evidence = host.init_failure_updates();
    let flipped = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        evidence.wait_for(|v| *v),
    )
    .await;
    assert!(
        flipped.is_ok() && flipped.unwrap().is_ok(),
        "the stall monitor flips the evidence at the ~RPC scale, no call needed"
    );
    assert!(!host.is_initialized(), "the wedge never initialized");
    host.shutdown();

    // A merely SLOW boot: the stall flips the evidence (its one silent
    // stage outlives the window), then init lands and clears it.
    std::fs::write(
        &plugins,
        "await new Promise((r) => setTimeout(r, 2000));\nexport default [];\n",
    )
    .unwrap();
    let host = match PluginHost::spawn_with_timeouts(
        &root,
        &plugins,
        &config,
        false,
        SpawnTimeouts {
            init_wait: Some(std::time::Duration::from_secs(120)),
            stall: Some(std::time::Duration::from_millis(500)),
            ..Default::default()
        },
    )
    .await
    {
        Ok(h) => h,
        Err(e) => panic!("the embedded engine spawns: {e}"),
    };
    let mut evidence = host.init_failure_updates();
    assert!(tokio::time::timeout(
        std::time::Duration::from_secs(20),
        evidence.wait_for(|v| *v),
    )
    .await
    .is_ok_and(|r| r.is_ok()));
    // Init progressing clears the evidence (a milestone or init itself).
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            evidence.wait_for(|v| !*v),
        )
        .await
        .is_ok_and(|r| r.is_ok()),
        "init progress clears stall evidence"
    );
    let mut init = host.initialized_updates();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(20), init.wait_for(|v| *v),)
            .await
            .is_ok_and(|r| r.is_ok())
    );
    host.shutdown();
}

// A hook wedging the isolate in SYNCHRONOUS JS is interrupted by the per-call
// watchdog at the deadline: ONE call fails, the host survives.
#[tokio::test]
async fn synchronously_wedged_hook_is_terminated_at_its_deadline_and_the_host_survives() {
    let root = std::env::temp_dir().join(format!("oj-wedged-sync-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let plugins = root.join("oj.plugins.mjs");
    std::fs::write(
        &plugins,
        r#"export default [{
  name: "blocker",
  load(id) {
if (id.includes("__block__")) {
  for (;;) {}
}
return null;
  },
}];
"#,
    )
    .unwrap();
    let config = serde_json::json!({
        "config": { "root": root.display().to_string() },
        "env": { "command": "serve", "mode": "development" },
    })
    .to_string();
    let host = PluginHost::spawn_with_timeouts(
        &root,
        &plugins,
        &config,
        true,
        SpawnTimeouts {
            init_wait: Some(std::time::Duration::from_secs(30)),
            rpc: Some(std::time::Duration::from_secs(1)),
            ..Default::default()
        },
    )
    .await
    .expect("the embedded engine spawns");
    // Prove init landed (post-init transport is what is under test).
    host.load("warmup").await.expect("healthy host answers");

    let t0 = std::time::Instant::now();
    let err = host
        .load("__block__")
        .await
        .expect_err("a synchronous wedge fails at its own deadline");
    assert!(
        err.contains("timed out") && err.contains("OJ_PLUGIN_TIMEOUT"),
        "one call fails on its timeout, the host is kept: {err}"
    );
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(10),
        "bounded: {:?}",
        t0.elapsed()
    );
    // The wedge was terminated, not the host: later calls succeed.
    host.load("after")
        .await
        .expect("the host survives a terminated synchronous wedge");
    host.shutdown();
}

// Transport belt: a hook blocked in NATIVE code stops the scheduler entirely,
// so the belt declares the host GONE and the next call revives it; the blocked thread leaks until the block ends.
#[tokio::test]
async fn natively_blocked_hook_declares_the_host_gone_and_the_next_call_revives_it() {
    let root = std::env::temp_dir().join(format!("oj-wedged-native-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let plugins = root.join("oj.plugins.mjs");
    std::fs::write(
        &plugins,
        r#"import { execSync } from "node:child_process";
export default [{
  name: "native-blocker",
  load(id) {
if (id.includes("__block__")) {
  execSync("sleep 60");
}
return null;
  },
}];
"#,
    )
    .unwrap();
    let config = serde_json::json!({
        "config": { "root": root.display().to_string() },
        "env": { "command": "serve", "mode": "development" },
    })
    .to_string();
    let host = PluginHost::spawn_with_timeouts(
        &root,
        &plugins,
        &config,
        true,
        SpawnTimeouts {
            init_wait: Some(std::time::Duration::from_secs(30)),
            rpc: Some(std::time::Duration::from_secs(1)),
            ..Default::default()
        },
    )
    .await
    .expect("the embedded engine spawns");
    host.load("warmup").await.expect("healthy host answers");

    let t0 = std::time::Instant::now();
    let err = host
        .load("__block__")
        .await
        .expect_err("a native block must fail at the belt");
    assert!(
        err.contains("unresponsive") || err.contains("exited"),
        "the belt names the wedge: {err}"
    );
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(10),
        "bounded, not a blocked transport: {:?}",
        t0.elapsed()
    );
    // Gone but not terminally: the next call revives on a fresh generation.
    clear_respawn_spacing(&host);
    host.load("after")
        .await
        .expect("the next call revives the host and serves");
    assert_eq!(
        host.revive.lock().unwrap().attempts,
        1,
        "one respawn consumed"
    );
    // A second death inside the respawn spacing fails fast without a window.
    let generation = host.revive.lock().unwrap().generation;
    host.declare_gone("second test wedge", generation);
    let t1 = std::time::Instant::now();
    let err = host
        .load("again")
        .await
        .expect_err("inside the spacing the host stays gone");
    assert!(err.contains("exited"), "{err}");
    assert!(
        t1.elapsed() < std::time::Duration::from_millis(500),
        "fail-fast on a declared-gone host: {:?}",
        t1.elapsed()
    );
}

// A hook that merely never SETTLES fails only that call at the per-call
// deadline; only total scheduler silence declares the host gone.
#[tokio::test]
async fn hung_hook_promise_fails_only_that_call() {
    let root = std::env::temp_dir().join(format!("oj-hung-hook-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let plugins = root.join("oj.plugins.mjs");
    std::fs::write(
        &plugins,
        r#"export default [{
  name: "hanger",
  load(id) {
if (id.includes("__hang__")) return new Promise(() => {});
return null;
  },
}];
"#,
    )
    .unwrap();
    let config = serde_json::json!({
        "config": { "root": root.display().to_string() },
        "env": { "command": "serve", "mode": "development" },
    })
    .to_string();
    let host = PluginHost::spawn_with_timeouts(
        &root,
        &plugins,
        &config,
        true,
        SpawnTimeouts {
            init_wait: Some(std::time::Duration::from_secs(30)),
            rpc: Some(std::time::Duration::from_secs(1)),
            ..Default::default()
        },
    )
    .await
    .expect("the embedded engine spawns");
    host.load("warmup").await.expect("healthy host answers");

    // A concurrent healthy call proves the hang costs nobody else their
    // window while the hung one waits out its own deadline.
    let racer = std::sync::Arc::clone(&host);
    let healthy = tokio::spawn(async move { racer.load("alongside").await });
    let t0 = std::time::Instant::now();
    let err = host
        .load("__hang__")
        .await
        .expect_err("a never-settling hook fails at its own deadline");
    assert!(
        err.contains("timed out") && err.contains("OJ_PLUGIN_TIMEOUT"),
        "names the per-call timeout, not a wedge: {err}"
    );
    assert!(
        t0.elapsed() >= std::time::Duration::from_millis(900)
            && t0.elapsed() < std::time::Duration::from_secs(2),
        "fails at the deadline, before the belt: {:?}",
        t0.elapsed()
    );
    healthy
        .await
        .unwrap()
        .expect("a concurrent call is untouched by the hang");

    // The host was NOT declared gone: later calls succeed.
    host.load("after")
        .await
        .expect("the host survives an abandoned hook promise");
    host.shutdown();
}

// Plugin env writes land in a private shadow, NEVER in oj's real environment;
// the host's cwd IS the app root and a plugin's chdir is contained to the shadow.
#[tokio::test]
async fn plugin_env_writes_and_cwd_stay_inside_the_host_shadow() {
    let root = std::env::temp_dir().join(format!("oj-env-shadow-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let plugins = root.join("oj.plugins.mjs");
    std::fs::write(
        &plugins,
        "process.env.OJ_HOST_CWD_BEFORE = process.cwd();\n\
         process.chdir(\"/\");\n\
         process.env.OJ_HOST_CWD_AFTER = process.cwd();\n\
         export default [];\n",
    )
    .unwrap();
    let config = serde_json::json!({
        "config": { "root": root.display().to_string() },
        "env": { "command": "serve", "mode": "development" },
    })
    .to_string();
    let host = PluginHost::spawn_lazy(&root, &plugins, &config, None)
        .await
        .expect("the embedded engine spawns");
    let delta = host.env_delta().await;
    let before = delta
        .get("OJ_HOST_CWD_BEFORE")
        .expect("the write is visible in the host's own delta");
    let canonical_root = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
    assert!(
        *before == canonical_root.display().to_string() || *before == root.display().to_string(),
        "the host's cwd is the app root: {before}"
    );
    assert_eq!(
        delta.get("OJ_HOST_CWD_AFTER").map(String::as_str),
        Some("/"),
        "a plugin's chdir moves the host's SHADOW cwd"
    );
    assert!(
        std::env::var("OJ_HOST_CWD_BEFORE").is_err() && std::env::var("OJ_HOST_CWD_AFTER").is_err(),
        "a plugin's env write must never reach oj's real environment"
    );
    assert_ne!(
        std::env::current_dir().unwrap(),
        std::path::PathBuf::from("/"),
        "a plugin's chdir must not move oj's real cwd"
    );
    host.shutdown();
}

// Vite ordering: buildStart completes before any serving hook. The host is
// never told to buildStart; the hook-entry gate must run it, even under concurrent first loads.
#[tokio::test]
async fn build_start_settles_before_any_load_even_under_concurrent_first_calls() {
    let root = std::env::temp_dir().join(format!("oj-buildstart-gate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let plugins = root.join("oj.plugins.mjs");
    std::fs::write(
        &plugins,
        r#"let plan;
export default [{
  name: "closure-state",
  buildStart() {
// Slow on purpose: an ungated load lands well inside this window.
return new Promise((resolve) => setTimeout(() => {
  plan = { groups: "compiled-groups" };
  resolve();
}, 300));
  },
  load(id) {
if (id === "\0closure-barrel") return `export default ${JSON.stringify(plan.groups)};`;
return null;
  },
}];
"#,
    )
    .unwrap();
    let config = serde_json::json!({
        "config": { "root": root.display().to_string() },
        "env": { "command": "serve", "mode": "development" },
    })
    .to_string();
    let host = PluginHost::spawn_lazy(&root, &plugins, &config, None)
        .await
        .expect("the embedded engine spawns");
    let (a, b, c, d) = tokio::join!(
        host.load("\u{0}closure-barrel"),
        host.load("\u{0}closure-barrel"),
        host.load("\u{0}closure-barrel"),
        host.load("\u{0}closure-barrel"),
    );
    for (i, r) in [a, b, c, d].into_iter().enumerate() {
        let code = r
            .unwrap_or_else(|e| panic!("first load #{i} must not race buildStart: {e}"))
            .expect("the plugin claims the id");
        assert!(
            code.contains("compiled-groups"),
            "load #{i} must serve the buildStart-computed state: {code}"
        );
    }
    host.shutdown();
}

// Push channel end to end: { ojServeInfo } with the middleware port, ws send
// on the broadcast, server.restart() on the server-events channel.
#[tokio::test]
async fn push_channel_delivers_serve_info_ws_and_server_events() {
    let root = std::env::temp_dir().join(format!("oj-push-dispatch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let plugins = root.join("oj.plugins.mjs");
    std::fs::write(
        &plugins,
        r#"export default [{
  name: "pusher",
  configureServer(server) {
server.middlewares.use((req, res, next) => next());
server.ws.send("oj:probe", { n: 7 });
server.restart();
  },
}];
"#,
    )
    .unwrap();
    let config = serde_json::json!({
        "config": { "root": root.display().to_string() },
        "env": { "command": "serve", "mode": "development" },
    })
    .to_string();
    let host = PluginHost::spawn_lazy(&root, &plugins, &config, None)
        .await
        .expect("the embedded engine spawns");
    let (ws_tx, mut ws_rx) = tokio::sync::broadcast::channel(16);
    host.set_ws_sender(ws_tx);
    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
    host.set_server_events_sender(ev_tx);

    let mut serve_info = host.serve_info_updates();
    let pushed = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        serve_info.wait_for(Option::is_some),
    )
    .await
    .expect("the serve-info push arrives")
    .expect("watch alive");
    let info = pushed.expect("serve info present");
    assert!(
        info.middleware_port.is_some(),
        "a registered middleware reports its loopback port"
    );
    drop(pushed);

    // Senders were installed before init; configureServer ran before the
    // serve-info push, so both deliveries are already in.
    let payload = tokio::time::timeout(std::time::Duration::from_secs(10), ws_rx.recv())
        .await
        .expect("the ws push arrives")
        .expect("broadcast alive");
    assert!(
        payload.contains("oj:probe") && payload.contains("custom"),
        "{payload}"
    );
    let ev = tokio::time::timeout(std::time::Duration::from_secs(10), ev_rx.recv())
        .await
        .expect("the server event arrives")
        .expect("channel alive");
    assert_eq!(ev.get("action").and_then(|a| a.as_str()), Some("restart"));
    host.shutdown();
}

#[test]
fn extraction_deps_truncated_gates_only_on_the_flag() {
    let t = serde_json::json!({ "__ok": true, "__depsTruncated": true });
    assert!(extraction_deps_truncated(&t));
    assert!(!extraction_deps_truncated(
        &serde_json::json!({ "__ok": true })
    ));
    assert!(!extraction_deps_truncated(
        &serde_json::json!({ "__depsTruncated": "yes" })
    ));
}

#[test]
fn extraction_env_hash_tracks_vite_vars_and_node_env_only() {
    let base = || {
        vec![
            ("PATH".to_string(), "/bin".to_string()),
            ("VITE_API".to_string(), "a".to_string()),
            ("NODE_ENV".to_string(), "development".to_string()),
        ]
    };
    let h0 = extraction_env_hash(base().into_iter());
    // Order-independent.
    let mut rev = base();
    rev.reverse();
    assert_eq!(h0, extraction_env_hash(rev.into_iter()));
    // Unrelated variables do not churn the key.
    let mut plus = base();
    plus.push(("TERM".to_string(), "xterm".to_string()));
    assert_eq!(h0, extraction_env_hash(plus.into_iter()));
    // A VITE_* or NODE_ENV change does.
    let mut vite = base();
    vite[1].1 = "b".to_string();
    assert_ne!(h0, extraction_env_hash(vite.into_iter()));
    let mut node = base();
    node[2].1 = "production".to_string();
    assert_ne!(h0, extraction_env_hash(node.into_iter()));
}

// Vite (constants.ts DEFAULT_CONFIG_FILES): js, mjs, ts, cjs, mts, cts; with
// both a .ts and a .js present, Vite loads the .js.
#[test]
fn config_discovery_precedence_matches_vite() {
    let root = std::env::temp_dir().join(format!("oj-config-precedence-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let order = ["js", "mjs", "ts", "cjs", "mts", "cts"];
    for ext in order.iter().rev() {
        std::fs::write(
            root.join(format!("vite.config.{ext}")),
            "export default {};",
        )
        .unwrap();
    }
    for ext in order {
        assert_eq!(
            vite_config_file(&root, None),
            Some(root.join(format!("vite.config.{ext}"))),
            "with every later format present, .{ext} wins"
        );
        std::fs::remove_file(root.join(format!("vite.config.{ext}"))).unwrap();
    }
    assert_eq!(vite_config_file(&root, None), None);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn parse_reads_all_fields() {
    let json = serde_json::json!({
        "base": "/app/",
        "publicDir": "/abs/shared/public",
        "port": 3010,
        "host": "0.0.0.0",
        "define": { "__X__": "1" },
        "alias": { "@": "/src" },
        "headers": { "x-a": "b" }
    });
    let v = parse_vite_values(&json);
    assert_eq!(v.base.as_deref(), Some("/app/"));
    assert_eq!(v.public_dir, Some("/abs/shared/public".into()));
    assert_eq!(v.port, Some(3010));
    assert_eq!(v.host.as_deref(), Some("0.0.0.0"));
    assert!(v.define.unwrap().contains_key("__X__"));
    assert!(v.alias.unwrap().contains_key("@"));
    assert!(v.headers.unwrap().contains_key("x-a"));
}

#[test]
fn parse_tolerates_nulls_and_missing() {
    let v = parse_vite_values(&serde_json::json!({ "base": null, "port": null }));
    assert!(v.base.is_none());
    assert!(v.public_dir.is_none());
    assert!(v.port.is_none());
    assert!(v.define.is_none());
}

#[test]
fn merge_adopts_only_unset_fields() {
    let mut config = oj_config::OjConfig::default();
    let v = ViteValues {
        base: Some("/vite-base/".into()),
        public_dir: Some("shared/public".into()),
        port: Some(3010),
        host: Some("localhost".into()),
        hmr_disabled: false,
        fs_allow: None,
        fs_strict: None,
        watch_ignored: None,
        define: None,
        alias: None,
        headers: None,
        rollup_options: None,
        assets_inline_limit: None,
        proxy: None,
        dedupe: None,
        optimize_deps: None,
        build: None,
        oxc: None,
        esbuild: None,
        ssr: None,
        mode: None,
        resolve: None,
        raw_resolve: None,
        server_flags: None,
        css: None,
        env_prefix: None,
        env_dir: None,
        cors: None,
        allowed_hosts: None,
        preview: None,
        app_type: None,
        html: None,
    };
    merge_vite_values(&mut config, v);
    assert_eq!(config.base.as_deref(), Some("/vite-base/"));
    assert_eq!(config.public_dir, Some("shared/public".into()));
    assert_eq!(config.server.unwrap().port, Some(3010));
}

#[test]
fn merge_never_overrides_config() {
    let mut config = oj_config::OjConfig {
        base: Some("/oj-base/".into()),
        ..Default::default()
    };
    config.public_dir = Some("my-public".into());
    let v = ViteValues {
        base: Some("/vite-base/".into()),
        public_dir: Some("shared/public".into()),
        port: None,
        host: None,
        hmr_disabled: false,
        fs_allow: None,
        fs_strict: None,
        watch_ignored: None,
        define: None,
        alias: None,
        headers: None,
        rollup_options: None,
        assets_inline_limit: None,
        proxy: None,
        dedupe: None,
        optimize_deps: None,
        build: None,
        oxc: None,
        esbuild: None,
        ssr: None,
        mode: None,
        resolve: None,
        raw_resolve: None,
        server_flags: None,
        css: None,
        env_prefix: None,
        env_dir: None,
        cors: None,
        allowed_hosts: None,
        preview: None,
        app_type: None,
        html: None,
    };
    merge_vite_values(&mut config, v);
    assert_eq!(config.base.as_deref(), Some("/oj-base/"));
    assert_eq!(config.public_dir, Some("my-public".into()));
}

#[test]
fn merge_fills_partial_server_and_optimizer_configuration() {
    let mut config: oj_config::OjConfig = serde_json::from_value(serde_json::json!({
        "server": { "fs": { "strict": false }, "warmup": { "clientFiles": ["own.ts"] } },
        "optimizeDeps": { "exclude": ["own-dep"] }
    }))
    .unwrap();
    merge_vite_values(
        &mut config,
        parse_vite_values(&serde_json::json!({
            "fsAllow": ["../shared"], "fsStrict": true,
            "serverFlags": { "fsDeny": ["**/*.private"], "warmup": {
                "clientFiles": ["other.ts"], "ssrFiles": ["server.ts"] } },
            "optimizeDeps": { "include": ["dep"], "exclude": ["other-dep"], "noDiscovery": true,
                "rolldownOptions": { "transform": { "define": { "FLAG": "true" }, "target": "es2015" } } }
        })),
    );
    let fs = config.server.as_ref().unwrap().fs.as_ref().unwrap();
    assert_eq!(fs.strict, Some(false));
    assert_eq!(fs.allow.as_ref().unwrap(), &["../shared"]);
    assert_eq!(fs.deny.as_ref().unwrap(), &["**/*.private"]);
    assert_eq!(
        oj_config::server_warmup_files(&config),
        (vec!["own.ts".into()], vec!["server.ts".into()])
    );
    let od = config.optimize_deps.as_ref().unwrap();
    assert_eq!(od.exclude.as_ref().unwrap(), &["own-dep"]);
    assert_eq!(od.include.as_ref().unwrap(), &["dep"]);
    assert_eq!(od.no_discovery, Some(true));
    let opts = oj_config::optimize_deps_bundler_options(&config).unwrap();
    assert_eq!(opts["define"]["FLAG"], "true");
    assert_eq!(opts["target"], "es2015");
}

#[test]
fn merge_adopts_server_fs_strict() {
    // `server.fs.strict: false` in a vite config reaches oj's FsConfig (Vite
    // skips the allow check entirely when strict is off) even with no allow list.
    let v = parse_vite_values(&serde_json::json!({ "fsStrict": false }));
    assert_eq!(v.fs_strict, Some(false));
    let mut config = oj_config::OjConfig::default();
    merge_vite_values(&mut config, v);
    let fs = config.server.unwrap().fs.unwrap();
    assert_eq!(fs.strict, Some(false));
    assert!(fs.allow.is_none());

    // Alongside an allow list both land; an oj-side fs config still wins.
    let v = parse_vite_values(&serde_json::json!({ "fsStrict": true, "fsAllow": ["../shared"] }));
    let mut config = oj_config::OjConfig::default();
    merge_vite_values(&mut config, v);
    let fs = config.server.unwrap().fs.unwrap();
    assert_eq!(fs.strict, Some(true));
    assert_eq!(fs.allow.as_deref(), Some(&["../shared".to_string()][..]));
    let absent = parse_vite_values(&serde_json::json!({}));
    assert_eq!(absent.fs_strict, None);
}

#[test]
fn merge_adopts_proxy() {
    let mut config = oj_config::OjConfig::default();
    let v = ViteValues {
        proxy: Some(serde_json::json!({
            "/api": "http://localhost:3000",
            "/ws": { "target": "http://localhost:4000", "changeOrigin": true }
        })),
        ..Default::default()
    };
    merge_vite_values(&mut config, v);
    let proxy = config.server.unwrap().proxy.unwrap();
    assert_eq!(proxy.get("/api").unwrap().target(), "http://localhost:3000");
    assert_eq!(proxy.get("/ws").unwrap().target(), "http://localhost:4000");
    assert!(proxy.get("/ws").unwrap().change_origin());
}

#[test]
fn merge_adopts_rollup_options() {
    let mut config = oj_config::OjConfig::default();
    let v = ViteValues {
        rollup_options: Some(serde_json::json!({ "output": { "entryFileNames": "x/[name].js" } })),
        ..Default::default()
    };
    merge_vite_values(&mut config, v);
    let ro = oj_config::rolldown_options(&config).unwrap();
    assert_eq!(
        ro.pointer("/output/entryFileNames")
            .and_then(|v| v.as_str()),
        Some("x/[name].js")
    );
}

#[test]
fn parse_reads_build_block() {
    let v = parse_vite_values(&serde_json::json!({
        "build": { "outDir": "out", "sourcemap": true, "minify": false,
                   "cssCodeSplit": false, "target": "es2020", "ssr": "src/entry-server.ts" }
    }));
    let b = v.build.unwrap();
    assert_eq!(b["outDir"], "out");
    assert_eq!(b["sourcemap"], true);
    assert_eq!(b["ssr"], "src/entry-server.ts");
    assert!(parse_vite_values(&serde_json::json!({ "build": null }))
        .build
        .is_none());
}

#[test]
fn merge_adopts_build_fields_only_when_unset() {
    let mut config = oj_config::OjConfig {
        build: Some(oj_config::BuildConfig {
            out_dir: Some("oj-out".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let v = ViteValues {
        build: Some(serde_json::json!({
            "outDir": "vite-out", "sourcemap": true, "minify": false,
            "cssCodeSplit": false, "target": "es2020", "ssr": "src/server.ts"
        })),
        ..Default::default()
    };
    merge_vite_values(&mut config, v);
    let b = config.build.unwrap();
    assert_eq!(b.out_dir.as_deref(), Some("oj-out"), "oj.config wins");
    assert_eq!(b.sourcemap, Some(oj_config::BoolOrString::Bool(true)));
    assert_eq!(b.minify, Some(oj_config::BoolOrString::Bool(false)));
    assert_eq!(b.css_code_split, Some(false));
    assert_eq!(
        b.target.as_ref().map(|t| t.to_vec()),
        Some(vec!["es2020".to_string()])
    );
    assert_eq!(
        b.ssr,
        Some(oj_config::BoolOrString::Str("src/server.ts".into()))
    );
}

#[test]
fn merge_adopts_ssr_block_and_ssr_manifest() {
    let mut config = oj_config::OjConfig::default();
    let v = ViteValues {
        build: Some(serde_json::json!({ "ssr": true, "ssrManifest": true })),
        ssr: Some(serde_json::json!({ "noExternal": ["ui-kit"], "target": "webworker" })),
        ..Default::default()
    };
    merge_vite_values(&mut config, v);
    assert_eq!(
        oj_config::ssr_manifest_name(&config).as_deref(),
        Some(".vite/ssr-manifest.json")
    );
    let e = oj_config::ssr_externals(&config);
    assert!(e.webworker() && !e.is_external_pkg("ui-kit"));
}

#[test]
fn merge_adopts_vite_string_variants_and_empty_out_dir() {
    let mut config = oj_config::OjConfig::default();
    let v = ViteValues {
        build: Some(serde_json::json!({
            "sourcemap": "hidden", "minify": "terser", "target": ["es2020", "safari14"],
            "emptyOutDir": false
        })),
        ..Default::default()
    };
    merge_vite_values(&mut config, v);
    assert_eq!(
        oj_config::build_sourcemap(&config),
        oj_config::Sourcemap::Hidden
    );
    assert!(oj_config::build_minify(&config));
    assert_eq!(
        oj_config::build_targets(&config),
        vec!["es2020", "safari14"]
    );
    assert_eq!(config.build.unwrap().empty_out_dir, Some(false));
}

#[test]
fn merge_ignores_build_values_of_the_wrong_shape() {
    let mut config = oj_config::OjConfig::default();
    let v = ViteValues {
        build: Some(serde_json::json!({ "outDir": 3, "sourcemap": 7, "target": {"x": 1} })),
        ..Default::default()
    };
    merge_vite_values(&mut config, v);
    let b = config.build.unwrap();
    assert!(b.out_dir.is_none());
    assert!(b.sourcemap.is_none());
    assert!(b.target.is_none());
}

#[test]
fn merge_adopts_jsx_blocks_when_unset() {
    let mut config = oj_config::OjConfig::default();
    let v = ViteValues {
        oxc: Some(serde_json::json!({ "jsx": { "importSource": "@emotion/react" } })),
        esbuild: Some(serde_json::json!({ "jsxFactory": "h" })),
        ..Default::default()
    };
    merge_vite_values(&mut config, v);
    let s = oj_config::jsx_settings(&config);
    assert_eq!(s.import_source.as_deref(), Some("@emotion/react"));
    assert_eq!(s.pragma.as_deref(), Some("h"));

    let mut config = oj_config::OjConfig {
        oxc: Some(serde_json::json!({ "jsx": { "importSource": "preact" } })),
        ..Default::default()
    };
    let v = ViteValues {
        oxc: Some(serde_json::json!({ "jsx": { "importSource": "@emotion/react" } })),
        ..Default::default()
    };
    merge_vite_values(&mut config, v);
    assert_eq!(
        oj_config::jsx_settings(&config).import_source.as_deref(),
        Some("preact"),
        "oj.config wins"
    );
}

#[test]
fn merge_adopts_ssr_block_when_unset() {
    let mut config = oj_config::OjConfig::default();
    let v = ViteValues {
        ssr: Some(
            serde_json::json!({ "noExternal": ["lodash-es", { "regex": "^@acme/" }], "external": ["sharp"] }),
        ),
        ..Default::default()
    };
    merge_vite_values(&mut config, v);
    let r = oj_config::ssr_externals(&config);
    assert!(r.is_no_external("lodash-es"));
    assert!(r.is_no_external("@acme/ui"));
    assert_eq!(r.is_external("sharp", true), Some(true));
}

#[test]
fn merge_adopts_resolve_server_css_env_and_mode() {
    let mut config = oj_config::OjConfig::default();
    let v = ViteValues {
        mode: Some("staging".into()),
        resolve: Some(serde_json::json!({
            "extensions": [".ts", ".js"], "mainFields": ["module"],
            "conditions": ["custom"], "externalConditions": ["custom-ext"],
            "preserveSymlinks": true
        })),
        server_flags: Some(serde_json::json!({ "strictPort": true, "open": true })),
        css: Some(
            serde_json::json!({ "preprocessorOptions": { "scss": { "additionalData": "@use 'x';" } } }),
        ),
        env_prefix: Some(vec!["VITE_".into(), "APP_".into()]),
        env_dir: Some("env".into()),
        ..Default::default()
    };
    merge_vite_values(&mut config, v);
    assert_eq!(config.mode.as_deref(), Some("staging"));
    let rc = config.resolve.as_ref().unwrap();
    assert_eq!(
        rc.extensions.as_deref(),
        Some(&[".ts".to_string(), ".js".to_string()][..])
    );
    assert_eq!(rc.main_fields.as_deref(), Some(&["module".to_string()][..]));
    assert_eq!(rc.conditions.as_deref(), Some(&["custom".to_string()][..]));
    assert_eq!(
        rc.external_conditions.as_deref(),
        Some(&["custom-ext".to_string()][..])
    );
    assert_eq!(rc.preserve_symlinks, Some(true));
    let sc = config.server.as_ref().unwrap();
    assert_eq!(sc.strict_port, Some(true));
    assert_eq!(sc.open, Some(true));
    let scss = &config
        .css
        .as_ref()
        .unwrap()
        .preprocessor_options
        .as_ref()
        .unwrap()["scss"];
    assert_eq!(scss.additional_data.as_deref(), Some("@use 'x';"));
    assert_eq!(
        oj_config::env_prefixes(&config),
        vec!["VITE_".to_string(), "APP_".to_string()]
    );
    assert_eq!(config.env_dir.as_deref(), Some("env"));

    // oj.config values win.
    let mut config = oj_config::OjConfig {
        mode: Some("qa".into()),
        ..Default::default()
    };
    config.env_dir = Some("cfg".into());
    merge_vite_values(
        &mut config,
        ViteValues {
            mode: Some("staging".into()),
            env_dir: Some("env".into()),
            ..Default::default()
        },
    );
    assert_eq!(config.mode.as_deref(), Some("qa"));
    assert_eq!(config.env_dir.as_deref(), Some("cfg"));
}

#[test]
fn merge_adopts_cors_and_allowed_hosts() {
    let mut config = oj_config::OjConfig::default();
    let v = ViteValues {
        cors: Some(serde_json::json!({ "origin": ["http://a.test"], "credentials": true })),
        allowed_hosts: Some(serde_json::json!([".corp.example"])),
        ..Default::default()
    };
    merge_vite_values(&mut config, v);
    let sc = config.server.unwrap();
    assert!(
        matches!(sc.cors, Some(oj_config::CorsConfig::Options(ref o)) if o.credentials == Some(true))
    );
    assert!(
        matches!(sc.allowed_hosts, Some(oj_config::AllowedHosts::List(ref l)) if l == &vec![".corp.example".to_string()])
    );
    let mut config = oj_config::OjConfig::default();
    merge_vite_values(
        &mut config,
        ViteValues {
            cors: Some(serde_json::json!(false)),
            allowed_hosts: Some(serde_json::json!(true)),
            ..Default::default()
        },
    );
    let sc = config.server.unwrap();
    assert!(matches!(
        sc.cors,
        Some(oj_config::CorsConfig::Toggle(false))
    ));
    assert!(matches!(
        sc.allowed_hosts,
        Some(oj_config::AllowedHosts::All(true))
    ));
}

#[test]
fn merge_adopts_css_preprocessor_options() {
    let mut config = oj_config::OjConfig::default();
    let v = ViteValues {
        css: Some(
            serde_json::json!({ "preprocessorOptions": { "scss": { "additionalData": "$b: red;", "loadPaths": ["styles"] } } }),
        ),
        ..Default::default()
    };
    merge_vite_values(&mut config, v);
    assert_eq!(
        oj_config::css_additional_data(&config, "scss").as_deref(),
        Some("$b: red;")
    );
    assert_eq!(
        oj_config::css_load_paths(&config, "scss"),
        vec!["styles".to_string()]
    );
}

#[test]
fn extraction_stderr_lines_print_once_per_process() {
    let first = unseen_extraction_lines(
        "oj: vite.config: worker config is not applied\nsome plugin notice\n",
    );
    assert_eq!(
        first,
        "oj: vite.config: worker config is not applied\nsome plugin notice\n"
    );
    let again = unseen_extraction_lines(
        "oj: vite.config: worker config is not applied\nsome plugin notice\nnew line\n",
    );
    assert_eq!(
        again, "new line\n",
        "only lines not printed before in this process come back"
    );
    assert_eq!(unseen_extraction_lines(""), "");
}

#[test]
fn parse_reads_watch_ignored() {
    let v =
        parse_vite_values(&serde_json::json!({ "watchIgnored": ["**/.wrangler/**", ".dev.vars"] }));
    assert_eq!(
        v.watch_ignored,
        Some(vec!["**/.wrangler/**".to_string(), ".dev.vars".to_string()])
    );
    let none = parse_vite_values(&serde_json::json!({ "watchIgnored": null }));
    assert!(none.watch_ignored.is_none());
}

#[test]
fn merge_unions_watch_ignored() {
    // Unlike the fill-if-unset fields, ignores union (Vite's mergeConfig
    // concatenates them): a user list must not displace the plugin-added
    // `**/.wrangler/**`, whose loss feeds miniflare state writes back into
    // the watcher.
    let mut config = oj_config::OjConfig::default();
    let sc = config.server.get_or_insert_with(Default::default);
    sc.watch = Some(oj_config::WatchConfig {
        ignored: Some(vec!["**/generated/**".to_string(), ".dev.vars".to_string()]),
    });
    let v = ViteValues {
        watch_ignored: Some(vec!["**/.wrangler/**".to_string(), ".dev.vars".to_string()]),
        ..Default::default()
    };
    merge_vite_values(&mut config, v);
    assert_eq!(
        config.server.unwrap().watch.unwrap().ignored,
        Some(vec![
            "**/generated/**".to_string(),
            ".dev.vars".to_string(),
            "**/.wrangler/**".to_string(),
        ]),
        "vite entries append without duplicating"
    );

    let mut unset = oj_config::OjConfig::default();
    let v = ViteValues {
        watch_ignored: Some(vec!["**/.wrangler/**".to_string()]),
        ..Default::default()
    };
    merge_vite_values(&mut unset, v);
    assert_eq!(
        unset.server.unwrap().watch.unwrap().ignored,
        Some(vec!["**/.wrangler/**".to_string()])
    );
}
