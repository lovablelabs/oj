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
        crate::plugins::test_roots::remove_root(&root);
    }
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

// The extractor reports how the app applies tsconfig `paths`: the plugin,
// Vite 8's `resolve.tsconfigPaths`, or neither.
#[test]
fn the_tsconfig_paths_mode_follows_the_extracted_config() {
    let mode = |resolve: serde_json::Value| {
        let mut config = oj_config::OjConfig::default();
        merge_vite_values(
            &mut config,
            parse_vite_values(&serde_json::json!({ "resolve": resolve })),
        );
        crate::tsconfig_paths(&config)
    };
    use oj_resolver::TsconfigPaths::*;
    let both = serde_json::json!({ "tsconfigPathsPlugin": true, "tsconfigPaths": true });
    assert_eq!(mode(both), PluginNative);
    let plugin = serde_json::json!({ "tsconfigPathsPlugin": true, "tsconfigPaths": false });
    assert_eq!(mode(plugin), Plugin);
    let native = serde_json::json!({ "tsconfigPathsPlugin": false, "tsconfigPaths": true });
    assert_eq!(mode(native), Native);
    let neither = serde_json::json!({ "tsconfigPathsPlugin": false, "tsconfigPaths": false });
    assert_eq!(mode(neither), Neither);
    assert_eq!(
        crate::tsconfig_paths(&oj_config::OjConfig::default()),
        Neither,
        "no host: no plugin, as Vite with no config"
    );
}

// The resolved config's alias list is the one Vite matches against: its
// order (mergeAlias puts a plugin's entries first when either side is an
// array) and values lead; a statically read entry it lacks follows.
#[test]
fn the_resolved_alias_list_leads_in_its_order() {
    let mut config: oj_config::OjConfig = serde_json::from_str(
        r#"{ "resolve": { "alias": [{ "find": "@", "replacement": "/src" }, { "find": "re", "replacement": "/re" }] } }"#,
    )
    .unwrap();
    let alias: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(r#"{ "@/lib": "/lib2", "@": "/abs/src" }"#).unwrap();
    merge_vite_values(
        &mut config,
        ViteValues {
            alias: Some(alias),
            ..Default::default()
        },
    );
    assert_eq!(
        oj_config::top_level_alias(&config),
        [("@/lib", "/lib2"), ("@", "/abs/src"), ("re", "/re")]
            .map(|(f, r)| (f.to_string(), r.to_string()))
    );
}

// The client environment's `resolve.dedupe` is the top-level list plus what
// `environments.client` and plugins' `configEnvironment` hooks add (TanStack
// Start's React and router), as Vite merges them; the client resolver takes
// it, and the ssr environment does not inherit the client's additions.
#[test]
fn the_client_dedupe_list_takes_the_client_environments_additions() {
    let mut config: oj_config::OjConfig = serde_json::from_str(
        r#"{ "environments": { "client": { "resolve": { "dedupe": ["from-file"] } } } }"#,
    )
    .unwrap();
    merge_vite_values(
        &mut config,
        parse_vite_values(&serde_json::json!({
            "dedupe": ["react"],
            "clientDedupe": ["from-file", "router"]
        })),
    );
    assert_eq!(
        oj_config::environment_dedupe(&config, "client"),
        ["react", "from-file", "router"]
    );
    assert_eq!(oj_config::resolve_dedupe(&config), ["react"]);
    assert_eq!(oj_config::environment_dedupe(&config, "ssr"), ["react"]);

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    for dir in [
        "node_modules/router",
        "node_modules/host/node_modules/router",
    ] {
        let dir = root.join(dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("package.json"),
            r#"{"name":"router","main":"index.js"}"#,
        )
        .unwrap();
        std::fs::write(dir.join("index.js"), "export {};\n").unwrap();
    }
    let resolved = crate::client_resolver(&root, &config)
        .resolve(&root.join("node_modules/host"), "router")
        .unwrap();
    assert_eq!(resolved, root.join("node_modules/router/index.js"));
}

#[test]
fn the_ssr_dedupe_list_takes_the_apps_own_ssr_environment_list() {
    let mut config = oj_config::OjConfig::default();
    merge_vite_values(
        &mut config,
        parse_vite_values(&serde_json::json!({
            "dedupe": ["react"],
            "ssrDedupe": ["shared"]
        })),
    );
    assert_eq!(
        oj_config::environment_dedupe(&config, "ssr"),
        ["react", "shared"]
    );
    assert_eq!(oj_config::environment_dedupe(&config, "client"), ["react"]);
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
    crate::plugins::test_roots::remove_root(&root);
    std::fs::create_dir_all(&root).unwrap();
    let plugins = root.join("oj.plugins.mjs");
    std::fs::write(&plugins, "export default [];\n").unwrap();
    let config = serde_json::json!({
        "config": { "root": root.display().to_string() },
        "env": { "command": "serve", "mode": "development" },
    })
    .to_string();
    let host = PluginHost::spawn(&root, &plugins, &config, None)
        .await
        .expect("the embedded engine spawns");
    host.resolve_id("x", "").await.expect("a live host serves");
    (root, host)
}

/// Rewind respawn spacing after an induced death: these tests target revive
/// semantics, not keeper timing.
fn clear_respawn_spacing(host: &PluginHost) {
    host.core.revive.lock().unwrap().last =
        Some(std::time::Instant::now() - PLUGIN_HOST_RESPAWN_SPACING);
}

// A hook rolldown calls runs on rolldown's runtime. A revive that hook
// triggers keeps the host's own work (the respawned engine, its spawns) on the
// runtime the host was spawned on.
#[test]
fn a_revive_inside_a_rolldown_build_keeps_the_hosts_runtime() {
    let rolldown = crate::rolldown_runtime::build().unwrap();
    let main = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let ran_on = main.block_on(async {
        let (_root, host) = spawn_live_host("rolldown-revive").await;
        let generation = host.core.revive.lock().unwrap().generation;
        host.core.declare_gone("test wedge", generation);
        clear_respawn_spacing(&host);
        crate::rolldown_runtime::on_rolldown(rolldown.handle(), host.resolve_id("x", ""))
            .await
            .expect("the call revives the host");
        assert_eq!(host.core.revive.lock().unwrap().generation, generation + 1);
        let runtime = host.runtime().expect("spawned on a runtime").clone();
        runtime
            .spawn(async { std::thread::current().name().map(str::to_owned) })
            .await
            .unwrap()
    });
    assert_ne!(ran_on.as_deref(), Some("oj-rolldown"));
}

// A wedge is no longer terminal: the next call revives the host with a
// fresh engine generation through the same boot path, and serves.
#[tokio::test]
async fn a_gone_host_is_revived_by_the_next_call() {
    let (_root, host) = spawn_live_host("basic").await;
    let generation = host.core.revive.lock().unwrap().generation;
    host.core.declare_gone("test wedge", generation);
    clear_respawn_spacing(&host);
    assert!(*host.core.host_gone.borrow(), "the death latched");

    host.resolve_id("x", "")
        .await
        .expect("the next call revives the host and serves");
    let revive = host.core.revive.lock().unwrap();
    assert_eq!(
        revive.generation,
        generation + 1,
        "a fresh engine generation"
    );
    assert_eq!(revive.attempts, 1, "one respawn consumed");
    drop(revive);
    assert!(!*host.core.host_gone.borrow(), "the host is live again");
}

// A revived engine numbers its worker containers afresh: a handle from the
// dead one answers nothing and closing it leaves the new container 1 alone.
#[tokio::test]
async fn a_worker_container_handle_dies_with_its_engine() {
    let (_root, host) = spawn_live_host("worker-gen").await;
    let old = host
        .open_worker_container(&[])
        .await
        .expect("a worker container");
    let generation = host.core.revive.lock().unwrap().generation;
    host.core.declare_gone("test wedge", generation);
    clear_respawn_spacing(&host);
    host.resolve_id("x", "").await.expect("revived");

    let new = host
        .open_worker_container(&[])
        .await
        .expect("a worker container");
    assert_eq!(
        old.container, new.container,
        "the revived engine reuses the id"
    );
    let err = old
        .resolve_id("x", "")
        .await
        .expect_err("the old handle is stale");
    assert!(err.contains("died with its plugin host's engine"), "{err}");
    old.close_container()
        .await
        .expect("closing a stale handle is a no-op");
    new.resolve_id("x", "")
        .await
        .expect("the new container is untouched by the stale close");
    new.close_container().await.expect("closed");
}

// An environment container is part of the host: a revived engine reopens it
// under the same id, on its plugins, and its gates refetch their filters.
#[tokio::test]
async fn a_revived_host_reopens_its_environment_containers() {
    let root = std::env::temp_dir().join(format!("oj-revive-env-{}", std::process::id()));
    crate::plugins::test_roots::remove_root(&root);
    std::fs::create_dir_all(&root).unwrap();
    let plugins = root.join("oj.plugins.mjs");
    std::fs::write(
        &plugins,
        r#"export default [{
  name: "ssr-md",
  applyToEnvironment: (environment) => environment.name === "ssr",
  transform: { filter: { id: /\.md$/ }, handler(code) { return code + " in " + this.environment.name; } },
}];
"#,
    )
    .unwrap();
    let config = serde_json::json!({
        "config": { "root": root.display().to_string() },
        "env": { "command": "serve", "mode": "development" },
    })
    .to_string();
    let host = PluginHost::spawn(&root, &plugins, &config, None)
        .await
        .expect("the embedded engine spawns");
    let ssr = host
        .open_environment_container("ssr", false, false)
        .await
        .expect("an ssr container");
    let md = root.join("a.md").display().to_string();
    let tsx = root.join("a.tsx").display().to_string();
    let served = |t: HostTransform| t.code;
    assert_eq!(
        served(ssr.transform("# a", &md, "").await.unwrap()),
        "# a in ssr"
    );
    assert_eq!(
        served(host.transform("# a", &md, "").await.unwrap()),
        "# a",
        "the client runs no ssr-only plugin"
    );

    let generation = host.core.revive.lock().unwrap().generation;
    host.core.declare_gone("test wedge", generation);
    clear_respawn_spacing(&host);
    assert_eq!(
        served(ssr.transform("# b", &md, "").await.expect("revived")),
        "# b in ssr",
        "the revived engine reopened the container"
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while ssr.hook_wants_transform(&tsx, "export default 1") {
        assert!(
            std::time::Instant::now() < deadline,
            "the reopened container never fetched its plan again"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

// An engine that wedges while an environment container opens is declared
// gone and comes back with it: the revived engine opens it from its seed,
// and the open adopts that container rather than failing (and leaving it
// orphaned there).
#[tokio::test]
async fn an_open_the_engine_dies_in_adopts_the_revived_container() {
    let root = std::env::temp_dir().join(format!("oj-revive-open-{}", std::process::id()));
    crate::plugins::test_roots::remove_root(&root);
    std::fs::create_dir_all(&root).unwrap();
    let flag = root.join("died-once");
    let plugins = root.join("oj.plugins.mjs");
    std::fs::write(
        &plugins,
        format!(
            r#"import fs from "node:fs";
export default [{{
  name: "dies-once",
  applyToEnvironment(environment) {{
    if (environment.name === "ssr" && !fs.existsSync({flag:?})) {{
      fs.writeFileSync({flag:?}, "");
      const until = Date.now() + 5000;
      while (Date.now() < until);
    }}
    return true;
  }},
  transform: {{ filter: {{ id: /\.md$/ }}, handler(code) {{ return code + " in " + this.environment.name; }} }},
}}];
"#
        ),
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
        SpawnTimeouts {
            probe: Some(std::time::Duration::from_millis(300)),
            ..Default::default()
        },
    )
    .await
    .expect("the embedded engine spawns");
    host.resolve_id("x", "").await.expect("a live host serves");
    clear_respawn_spacing(&host);
    let ssr = host
        .open_environment_container("ssr", false, false)
        .await
        .expect("the open survives the engine's death");
    let md = root.join("a.md").display().to_string();
    assert_eq!(
        ssr.transform("# a", &md, "").await.unwrap().code,
        "# a in ssr"
    );
    assert_eq!(
        host.core.environments.lock().unwrap().len(),
        1,
        "one container, the adopted one"
    );
}

// Another caller can revive the engine between the open's failure and its
// retry check: the revived engine reopened the container from its seed, and
// the open adopts it instead of closing it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_open_adopts_a_container_another_caller_revived() {
    let root = std::env::temp_dir().join(format!("oj-revive-race-{}", std::process::id()));
    crate::plugins::test_roots::remove_root(&root);
    std::fs::create_dir_all(&root).unwrap();
    let flag = root.join("died-once");
    let plugins = root.join("oj.plugins.mjs");
    std::fs::write(
        &plugins,
        format!(
            r#"import fs from "node:fs";
export default [{{
  name: "dies-once",
  applyToEnvironment(environment) {{
    if (environment.name === "ssr" && !fs.existsSync({flag:?})) {{
      fs.writeFileSync({flag:?}, "");
      const until = Date.now() + 5000;
      while (Date.now() < until);
    }}
    return true;
  }},
  transform: {{ filter: {{ id: /\.md$/ }}, handler(code) {{ return code + " in " + this.environment.name; }} }},
}}];
"#
        ),
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
        SpawnTimeouts {
            probe: Some(std::time::Duration::from_millis(300)),
            ..Default::default()
        },
    )
    .await
    .expect("the embedded engine spawns");
    host.resolve_id("x", "").await.expect("a live host serves");
    clear_respawn_spacing(&host);
    // The other caller: revives the engine the moment it is declared gone.
    let reviver = {
        let host = std::sync::Arc::clone(&host);
        let mut gone = host.host_gone_updates();
        tokio::spawn(async move {
            let _ = gone.wait_for(|g| *g).await;
            let _ = host.resolve_id("x", "").await;
        })
    };
    let ssr = host
        .open_environment_container("ssr", false, false)
        .await
        .expect("the open adopts the revived container");
    reviver.await.unwrap();
    let md = root.join("a.md").display().to_string();
    assert_eq!(
        ssr.transform("# a", &md, "").await.unwrap().code,
        "# a in ssr"
    );
}

// A revive held back by the respawn spacing fails a call at once: an open the
// engine dies in waits for the revive instead of spinning on retries, and
// still adopts the revived container.
#[tokio::test]
async fn an_open_waits_out_a_held_back_revive_without_spinning() {
    let root = std::env::temp_dir().join(format!("oj-revive-spacing-{}", std::process::id()));
    crate::plugins::test_roots::remove_root(&root);
    std::fs::create_dir_all(&root).unwrap();
    let flag = root.join("wedged-once");
    let plugins = root.join("oj.plugins.mjs");
    std::fs::write(
        &plugins,
        format!(
            r#"import fs from "node:fs";
export default [{{
  name: "wedges-once",
  applyToEnvironment(environment) {{
    if (environment.name === "ssr" && !fs.existsSync({flag:?})) {{
      fs.writeFileSync({flag:?}, "");
      const until = Date.now() + 1500;
      while (Date.now() < until);
    }}
    return true;
  }},
}}];
"#
        ),
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
        SpawnTimeouts {
            probe: Some(std::time::Duration::from_millis(200)),
            ..Default::default()
        },
    )
    .await
    .expect("the embedded engine spawns");
    host.resolve_id("x", "").await.expect("a live host serves");
    // A revive just ran: the next is held back for the respawn spacing.
    host.core.revive.lock().unwrap().last = Some(std::time::Instant::now());
    // The open's loop runs on this thread (a current-thread runtime), so this
    // thread's CPU is the wait's, whatever other tests run alongside.
    let cpu = || {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
        ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9
    };
    let (cpu0, wall0) = (cpu(), std::time::Instant::now());
    host.open_environment_container("ssr", false, false)
        .await
        .expect("the open adopts the container once the revive is allowed");
    let (spent, waited) = (cpu() - cpu0, wall0.elapsed().as_secs_f64());
    assert!(
        waited >= 3.0,
        "the spacing held the revive back ({waited:.1}s)"
    );
    assert!(
        spent < waited / 2.0,
        "{spent:.1}s of CPU over {waited:.1}s: the wait spun"
    );
}

// A revived engine resets the gates to fail-open, then fetches the plan
// again: the hooks are gated by their filters on the new generation too.
#[tokio::test]
async fn a_revived_host_gates_by_the_plugins_filters_again() {
    let root = std::env::temp_dir().join(format!("oj-revive-plan-{}", std::process::id()));
    crate::plugins::test_roots::remove_root(&root);
    std::fs::create_dir_all(&root).unwrap();
    let plugins = root.join("oj.plugins.mjs");
    std::fs::write(
        &plugins,
        "export default [{ name: \"md\", transform: { filter: { id: /\\.md$/ }, handler(code) { return code; } } }];\n",
    )
    .unwrap();
    let config = serde_json::json!({
        "config": { "root": root.display().to_string() },
        "env": { "command": "serve", "mode": "development" },
    })
    .to_string();
    let host = PluginHost::spawn(&root, &plugins, &config, None)
        .await
        .expect("the embedded engine spawns");
    host.build_hook_plan().await;
    let tsx = root.join("a.tsx").display().to_string();
    assert!(!host.hook_wants_transform(&tsx, "export default 1"));

    let generation = host.core.revive.lock().unwrap().generation;
    host.core.declare_gone("test wedge", generation);
    clear_respawn_spacing(&host);
    host.resolve_id("x", "").await.expect("revived");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while host.hook_wants_transform(&tsx, "export default 1") {
        assert!(
            std::time::Instant::now() < deadline,
            "the revived host never fetched its plan again"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(host.hook_wants_transform(&root.join("a.md").display().to_string(), "# a"));
}

// A death report about a replaced engine (an old call's transport belt
// firing after a revive) is stale and must not kill the new generation.
#[tokio::test]
async fn a_stale_death_report_does_not_kill_a_revived_host() {
    let (_root, host) = spawn_live_host("stale").await;
    let generation = host.core.revive.lock().unwrap().generation;
    host.core.declare_gone("test wedge", generation);
    clear_respawn_spacing(&host);
    host.resolve_id("x", "").await.expect("revived");

    host.core
        .declare_gone("stale report about the old engine", generation);
    assert!(
        !*host.core.host_gone.borrow(),
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
    assert_eq!(
        host.core.revive.lock().unwrap().attempts,
        0,
        "no respawn burned"
    );
}

// Keeper is best effort per addon: an unloadable path is skipped, the eval
// succeeds, the keeper engine stays up for the next death.
#[tokio::test]
async fn addon_keeper_tolerates_unloadable_addons() {
    let root = std::env::temp_dir().join(format!("oj-keeper-{}", std::process::id()));
    crate::plugins::test_roots::remove_root(&root);
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

// The cap is V8's own figure for the machine, as Node's and Deno's is: a
// 4GB old space once the memory allows it with the young generation on top,
// less on a small machine, and `--max-old-space-size=N` that same young
// generation above N. A machine whose memory cannot be read keeps 4096MB.
#[test]
fn the_heap_cap_is_v8s_for_the_machine() {
    const GB: u64 = 1 << 30;
    let big = heap_cap_mb(Some(64 * GB), None);
    let young = big - 4096;
    assert!(
        (1..=1024).contains(&young),
        "a 4GB old space and a young generation: {big}MB"
    );
    assert_eq!(heap_cap_mb(Some(16 * GB), None), big);
    let small = heap_cap_mb(Some(2 * GB), None);
    assert!(
        (512..2048).contains(&small),
        "a 2GB machine's heap fits its memory: {small}MB"
    );
    assert_eq!(heap_cap_mb(Some(64 * GB), Some(8192)), 8192 + young);
    assert_eq!(heap_cap_mb(Some(64 * GB), Some(1024)), 1024 + young);
    assert_eq!(heap_cap_mb(None, None), 4096);
    assert_eq!(heap_cap_mb(None, Some(2048)), 2048);
}

// Heap cap mirrors NODE_OPTIONS --max-old-space-size (last wins, underscore
// accepted, like Node), V8's default for the machine beneath.
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
    crate::plugins::test_roots::remove_root(&root);
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
        SpawnTimeouts {
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
        err.contains("memory limit") && err.contains("--max-old-space-size"),
        "the failure names the cap and its knob: {err}"
    );
    assert!(
        *host.core.host_gone.borrow(),
        "a blown heap retires the engine"
    );

    clear_respawn_spacing(&host);
    host.load("after")
        .await
        .expect("the next call revives the host on a fresh heap");
    assert_eq!(
        host.core.revive.lock().unwrap().attempts,
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
        let generation = host.core.revive.lock().unwrap().generation;
        host.core.declare_gone("recurring test wedge", generation);
        if round == 0 {
            clear_respawn_spacing(&host);
        }
        // Immediately after a previous revive the spacing rejects the
        // attempt; backdate the clock instead of sleeping it out.
        if round > 0 {
            let err = host.resolve_id("x", "").await.expect_err("spacing rejects");
            assert!(err.contains("plugin host exited"), "{err}");
            host.core.revive.lock().unwrap().last =
                Some(std::time::Instant::now() - PLUGIN_HOST_RESPAWN_SPACING);
        }
        host.resolve_id("x", "")
            .await
            .expect("revives within budget");
    }
    let generation = host.core.revive.lock().unwrap().generation;
    host.core.declare_gone("one wedge too many", generation);
    host.core.revive.lock().unwrap().last =
        Some(std::time::Instant::now() - PLUGIN_HOST_RESPAWN_SPACING);
    let err = host.resolve_id("x", "").await.expect_err("budget spent");
    assert!(err.contains("plugin host exited"), "{err}");
    assert!(!host.can_revive(), "no revive left for waiters to hold on");
}

// The server-entry incident's shape: a lazy host whose init outlasts every
// probe window still serves the first transform-path call. Vite never fails
// a request on a slow plugin boot (requests only exist after init), and
// elapsed time alone fails nothing here either: the probes just tick while
// the call waits init out.
#[tokio::test]
async fn lazy_call_during_an_init_slower_than_the_probe_window_is_served() {
    let root = std::env::temp_dir().join(format!("oj-lazy-slow-init-{}", std::process::id()));
    crate::plugins::test_roots::remove_root(&root);
    std::fs::create_dir_all(&root).unwrap();
    // Init outlasts several 500ms probe windows, then completes.
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
    let host = PluginHost::spawn_with_timeouts(
        &root,
        &plugins,
        &config,
        SpawnTimeouts {
            probe: Some(std::time::Duration::from_millis(500)),
            ..Default::default()
        },
    )
    .await
    .expect("the embedded engine spawns");
    let t0 = std::time::Instant::now();
    host.resolve_id("x", "")
        .await
        .expect("a call during a slow init waits it out and is served");
    assert!(
        t0.elapsed() >= std::time::Duration::from_millis(2300),
        "the call waited for init rather than being answered early: {:?}",
        t0.elapsed()
    );
}

// The init wait is UNBOUNDED: elapsed time never fails a pre-init call (the
// Vite shape — a request only exists after initServer resolved). The stall
// monitor still flips wedge EVIDENCE for health waiters, and host death is
// what releases a waiting call.
#[tokio::test]
async fn pre_init_calls_never_fail_on_elapsed_time_only_on_host_death() {
    let root = std::env::temp_dir().join(format!("oj-lazy-window-{}", std::process::id()));
    crate::plugins::test_roots::remove_root(&root);
    std::fs::create_dir_all(&root).unwrap();
    // Init never completes; the interval keeps the event loop alive.
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
    let host = match PluginHost::spawn_with_timeouts(
        &root,
        &plugins,
        &config,
        SpawnTimeouts {
            stall: Some(std::time::Duration::from_millis(300)),
            probe: Some(std::time::Duration::from_millis(300)),
            ..Default::default()
        },
    )
    .await
    {
        Ok(h) => h,
        Err(e) => panic!("the embedded engine spawns: {e}"),
    };
    let mut evidence = host.init_failure_updates();

    // Far past any OS pipe buffer (the old transport's wedge trigger), and
    // two concurrent calls: pre-init calls must submit NOTHING and simply
    // wait, not serialize behind — or block on — the wedged engine.
    let big = "x".repeat(2 * 1024 * 1024);
    let waiting = {
        let host = std::sync::Arc::clone(&host);
        let big = big.clone();
        tokio::spawn(
            async move { tokio::join!(host.resolve_id(&big, ""), host.resolve_id(&big, "")) },
        )
    };
    // Many former windows later the calls are still waiting…
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    assert!(
        !waiting.is_finished(),
        "elapsed time alone never fails a pre-init call"
    );
    // …while the stall monitor flipped the evidence with no call's help.
    assert!(
        *evidence.borrow_and_update(),
        "the stall monitor owns wedge evidence"
    );

    // Host death is what fails the waiters, and promptly.
    host.shutdown();
    let t0 = std::time::Instant::now();
    let (a, b) = tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
        .await
        .expect("death releases the waiters")
        .unwrap();
    for res in [a, b] {
        let err = res.expect_err("a dead host fails its waiting calls");
        assert!(err.contains("plugin host exited"), "{err}");
    }
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(2),
        "waiters release on the death flip (engine teardown latency), not a timer: {:?}",
        t0.elapsed()
    );
}

// Stall monitor is the boot host's real evidence flip site: a wedge flips
// evidence at the stall window with NO call in flight; a merely slow host flips then clears on init.
#[tokio::test]
async fn boot_host_stall_monitor_flips_evidence_without_a_call_and_init_clears_it() {
    let root = std::env::temp_dir().join(format!("oj-boot-stall-{}", std::process::id()));
    crate::plugins::test_roots::remove_root(&root);
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
        SpawnTimeouts {
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
        SpawnTimeouts {
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

// A hook wedging the isolate in SYNCHRONOUS JS silences the liveness probes
// (the event loop cannot answer the ping): the host is declared gone and the
// next call revives it. Vite would hang forever on the same plugin; oj pays
// one respawn instead, and no timer ever kills healthy work to get there.
#[tokio::test]
async fn synchronously_wedged_hook_silences_the_probes_and_the_host_is_revived() {
    let root = std::env::temp_dir().join(format!("oj-wedged-sync-{}", std::process::id()));
    crate::plugins::test_roots::remove_root(&root);
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
        SpawnTimeouts {
            probe: Some(std::time::Duration::from_millis(500)),
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
        .expect_err("a synchronous wedge silences the probes");
    assert!(
        err.contains("unresponsive") || err.contains("exited"),
        "the probe names the wedge: {err}"
    );
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(10),
        "bounded by the probe, not a hook budget: {:?}",
        t0.elapsed()
    );
    // Gone but not terminally: the next call revives on a fresh generation.
    clear_respawn_spacing(&host);
    host.load("after")
        .await
        .expect("the next call revives the host and serves");
    host.shutdown();
}

// A hook blocked in NATIVE code stops the scheduler entirely, so the probe
// goes unanswered and declares the host GONE; the next call revives it (the
// blocked thread leaks until the block ends).
#[tokio::test]
async fn natively_blocked_hook_declares_the_host_gone_and_the_next_call_revives_it() {
    let root = std::env::temp_dir().join(format!("oj-wedged-native-{}", std::process::id()));
    crate::plugins::test_roots::remove_root(&root);
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
        SpawnTimeouts {
            probe: Some(std::time::Duration::from_millis(500)),
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
        .expect_err("a native block must fail at the probe");
    assert!(
        err.contains("unresponsive") || err.contains("exited"),
        "the probe names the wedge: {err}"
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
        host.core.revive.lock().unwrap().attempts,
        1,
        "one respawn consumed"
    );
    // A second death inside the respawn spacing fails fast without a window.
    let generation = host.core.revive.lock().unwrap().generation;
    host.core.declare_gone("second test wedge", generation);
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

// A hook that merely never SETTLES leaves its event loop parked, so the
// probes keep answering and the call just WAITS — Vite's exact behavior for
// a never-resolving plugin hook. Nobody else is affected, no timer fires,
// and host death is what would release the waiter.
#[tokio::test]
async fn hung_hook_promise_waits_and_the_host_keeps_serving() {
    let root = std::env::temp_dir().join(format!("oj-hung-hook-{}", std::process::id()));
    crate::plugins::test_roots::remove_root(&root);
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
        SpawnTimeouts {
            probe: Some(std::time::Duration::from_millis(300)),
            ..Default::default()
        },
    )
    .await
    .expect("the embedded engine spawns");
    host.load("warmup").await.expect("healthy host answers");

    // The hung call outlives MANY probe windows without failing: the parked
    // loop answers every ping, so no timer and no wedge verdict fires.
    let hung = {
        let host = std::sync::Arc::clone(&host);
        tokio::spawn(async move { host.load("__hang__").await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    assert!(
        !hung.is_finished(),
        "a never-settling hook is waited on, never timed out"
    );
    // The host keeps serving everyone else while the hang waits.
    host.load("alongside")
        .await
        .expect("a concurrent call is untouched by the hang");
    assert!(
        !*host.core.host_gone.borrow(),
        "no wedge verdict for a parked loop"
    );

    // Host death releases the waiter promptly.
    host.shutdown();
    let released = tokio::time::timeout(std::time::Duration::from_secs(5), hung)
        .await
        .expect("death releases the hung waiter")
        .unwrap();
    let err = released.expect_err("a dead host fails its waiting call");
    assert!(err.contains("exited"), "{err}");
}

// Plugin env writes land in a private shadow, NEVER in oj's real environment;
// the host's cwd IS the app root and a plugin's chdir is contained to the shadow.
#[tokio::test]
async fn plugin_env_writes_and_cwd_stay_inside_the_host_shadow() {
    let root = std::env::temp_dir().join(format!("oj-env-shadow-{}", std::process::id()));
    crate::plugins::test_roots::remove_root(&root);
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
    let host = PluginHost::spawn(&root, &plugins, &config, None)
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
    // The real cwd is wherever the last host booted (another test's root,
    // possibly gone by now): anything but the plugin's "/".
    assert!(
        std::env::current_dir().map_or(true, |cwd| cwd != std::path::Path::new("/")),
        "a plugin's chdir must not move oj's real cwd"
    );
    host.shutdown();
}

// Vite ordering: buildStart completes before any serving hook. The host is
// never told to buildStart; the hook-entry gate must run it, even under concurrent first loads.
#[tokio::test]
async fn build_start_settles_before_any_load_even_under_concurrent_first_calls() {
    let root = std::env::temp_dir().join(format!("oj-buildstart-gate-{}", std::process::id()));
    crate::plugins::test_roots::remove_root(&root);
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
    let host = PluginHost::spawn(&root, &plugins, &config, None)
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
    crate::plugins::test_roots::remove_root(&root);
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
    let host = PluginHost::spawn(&root, &plugins, &config, None)
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
    crate::plugins::test_roots::remove_root(&root);
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
    crate::plugins::test_roots::remove_root(&root);
}

#[test]
fn parse_reads_all_fields() {
    let json = serde_json::json!({
        "base": "/app/",
        "publicDir": "/abs/shared/public",
        "cacheDir": "/abs/shared/node_modules/.vite",
        "port": 3010,
        "host": "0.0.0.0",
        "define": { "__X__": "1" },
        "alias": { "@": "/src" },
        "headers": { "x-a": "b" }
    });
    let v = parse_vite_values(&json);
    assert_eq!(v.base.as_deref(), Some("/app/"));
    assert_eq!(v.public_dir, Some("/abs/shared/public".into()));
    assert_eq!(
        v.cache_dir.as_deref(),
        Some("/abs/shared/node_modules/.vite")
    );
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
    assert!(v.cache_dir.is_none());
    assert!(v.port.is_none());
    assert!(v.define.is_none());
}

#[test]
fn merge_adopts_only_unset_fields() {
    let mut config = oj_config::OjConfig::default();
    let v = ViteValues {
        base: Some("/vite-base/".into()),
        public_dir: Some("shared/public".into()),
        cache_dir: None,
        port: Some(3010),
        host: Some("localhost".into()),
        origin: Some("https://cdn.example".into()),
        hmr_disabled: false,
        fs_allow: None,
        fs_strict: None,
        watch_ignored: None,
        define: None,
        alias: None,
        headers: None,
        rollup_options: None,
        assets_inline_limit: None,
        assets_inline_limit_fn: false,
        assets_inline_limit_envs: None,
        proxy: None,
        dedupe: None,
        client_dedupe: None,
        ssr_dedupe: None,
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
        assets_include: None,
        active_plugins: None,
        stylex: None,
        editor_hmr_gate: false,
        start_router: None,
        resolve_error: None,
    };
    merge_vite_values(&mut config, v);
    assert_eq!(config.base.as_deref(), Some("/vite-base/"));
    assert_eq!(config.public_dir, Some("shared/public".into()));
    let server = config.server.unwrap();
    assert_eq!(server.port, Some(3010));
    assert_eq!(server.origin.as_deref(), Some("https://cdn.example"));
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
        cache_dir: None,
        port: None,
        host: None,
        origin: None,
        hmr_disabled: false,
        fs_allow: None,
        fs_strict: None,
        watch_ignored: None,
        define: None,
        alias: None,
        headers: None,
        rollup_options: None,
        assets_inline_limit: None,
        assets_inline_limit_fn: false,
        assets_inline_limit_envs: None,
        proxy: None,
        dedupe: None,
        client_dedupe: None,
        ssr_dedupe: None,
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
        assets_include: None,
        active_plugins: None,
        stylex: None,
        editor_hmr_gate: false,
        start_router: None,
        resolve_error: None,
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
                "holdUntilCrawlEnd": false,
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
    assert_eq!(od.hold_until_crawl_end, Some(false));
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

// The extractor reports the resolved config's `legacy` flags in `serverFlags`.
#[test]
fn merge_adopts_the_legacy_flags() {
    let mut config = oj_config::OjConfig::default();
    let v = ViteValues {
        server_flags: Some(serde_json::json!({
            "skipWebSocketTokenCheck": true,
            "inconsistentCjsInterop": true
        })),
        ..Default::default()
    };
    merge_vite_values(&mut config, v);
    let legacy = config.legacy.as_ref().unwrap();
    assert_eq!(legacy.skip_web_socket_token_check, Some(true));
    assert_eq!(legacy.inconsistent_cjs_interop, Some(true));
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

// `assetsInclude` reaches the config as the extractor carries it: string
// globs and RegExps as `{ regex, flags }`; an empty list is no value.
#[test]
fn merge_adopts_assets_include() {
    let mut config = oj_config::OjConfig::default();
    let patterns = vec![
        serde_json::json!("**/*.gltf"),
        serde_json::json!({ "regex": "\\.bin$", "flags": "" }),
    ];
    merge_vite_values(
        &mut config,
        ViteValues {
            assets_include: Some(patterns.clone()),
            ..Default::default()
        },
    );
    assert_eq!(config.assets_include, Some(patterns));
    let mut empty = oj_config::OjConfig::default();
    merge_vite_values(
        &mut empty,
        ViteValues {
            assets_include: Some(Vec::new()),
            ..Default::default()
        },
    );
    assert_eq!(empty.assets_include, None);
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

// The seen set is one per process, shared with every extraction another
// test in this binary runs: these lines are this test's own, so no real
// extraction's stderr (`worker_config_warns_only_in_an_spa_build` prints
// the worker warning) can have printed them first.
#[test]
fn extraction_stderr_lines_print_once_per_process() {
    let first = unseen_extraction_lines(
        "oj: vite.config: once-per-process test line\nonce-per-process plugin notice\n",
    );
    assert_eq!(
        first,
        "oj: vite.config: once-per-process test line\nonce-per-process plugin notice\n"
    );
    let again = unseen_extraction_lines(
        "oj: vite.config: once-per-process test line\nonce-per-process plugin notice\nonce-per-process new line\n",
    );
    assert_eq!(
        again, "once-per-process new line\n",
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

// The engine started ahead of the payload (dev boot overlaps it with the
// config extraction) becomes the host's first generation: the plugins it
// loads serve, and a revive still boots a fresh engine the ordinary way.
#[tokio::test]
async fn a_prewarmed_engine_hosts_the_plugins_and_revives_normally() {
    let root = std::env::temp_dir().join(format!("oj-prewarm-{}", std::process::id()));
    crate::plugins::test_roots::remove_root(&root);
    std::fs::create_dir_all(&root).unwrap();
    let plugins = root.join("oj.plugins.mjs");
    std::fs::write(
        &plugins,
        "export default [{ name: \"t\", resolveId(id) { if (id === \"virtual:x\") return \"\\0x\"; } }];\n",
    )
    .unwrap();
    let config = serde_json::json!({
        "config": { "root": root.display().to_string() },
        "env": { "command": "serve", "mode": "development" },
    })
    .to_string();
    // No vite under this root: the warm import fails quietly, as it must.
    let warm = PluginHost::prewarm(&root, None).expect("the engine starts ahead");
    let host = PluginHost::spawn_prewarmed(&root, &plugins, &config, warm)
        .await
        .expect("the host adopts the engine");
    let resolved = host
        .resolve_id("virtual:x", "")
        .await
        .expect("a live host serves");
    assert!(
        resolved
            .as_deref()
            .is_some_and(|r| r.ends_with('x') || r.contains("x\"")),
        "the plugin loaded on the prewarmed engine resolves: {resolved:?}"
    );

    let generation = host.core.revive.lock().unwrap().generation;
    host.core.declare_gone("test wedge", generation);
    clear_respawn_spacing(&host);
    host.resolve_id("virtual:x", "")
        .await
        .expect("a revive boots a fresh engine and serves");
    assert_eq!(host.core.revive.lock().unwrap().generation, generation + 1);
    crate::plugins::test_roots::remove_root(&root);
}

// The app's vite, warmed ahead of the host, is the module the host's
// loadViteConfig gets (evaluated once), and a timer it leaves running at
// import does not hold the host's boot: nothing on the host's engine waits
// for its event loop to go idle.
#[tokio::test]
async fn a_prewarmed_vite_is_shared_and_its_timers_never_hold_the_boot() {
    let root = std::env::temp_dir().join(format!("oj-prewarm-vite-{}", std::process::id()));
    crate::plugins::test_roots::remove_root(&root);
    let vite = root.join("node_modules/vite");
    std::fs::create_dir_all(&vite).unwrap();
    std::fs::write(
        vite.join("package.json"),
        r#"{"name":"vite","version":"8.0.0","type":"module","exports":{".":"./index.js"}}"#,
    )
    .unwrap();
    std::fs::write(
        vite.join("index.js"),
        r#"import { appendFileSync } from "node:fs";
appendFileSync(new URL("./evals.log", import.meta.url), "x");
setInterval(() => {}, 1000);
export async function loadConfigFromFile() {
  return { config: { plugins: [{ name: "t", resolveId(id) { if (id === "virtual:x") return "\0x"; } }] } };
}
"#,
    )
    .unwrap();
    let plugins = root.join("vite.config.mjs");
    std::fs::write(&plugins, "export default {};\n").unwrap();
    let config = serde_json::json!({
        "config": { "root": root.display().to_string() },
        "env": { "command": "serve", "mode": "development" },
        "pluginsFormat": "vite",
    })
    .to_string();
    let evals = vite.join("evals.log");

    let warm = PluginHost::prewarm(&root, None).expect("the engine starts ahead");
    // The dev server adopts the engine after the config extraction, by when
    // the warm import has run and its interval is live.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !evals.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the warm import never ran"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let host = PluginHost::spawn_prewarmed(&root, &plugins, &config, warm)
        .await
        .expect("the host adopts the engine");
    let resolved = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        host.resolve_id("virtual:x", ""),
    )
    .await
    .expect("the host boots while vite's interval runs")
    .expect("a live host serves");
    assert!(
        resolved.as_deref().is_some_and(|r| r.ends_with('x')),
        "the plugin from the warmed vite resolves: {resolved:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&evals).unwrap(),
        "x",
        "vite evaluates once"
    );
    host.shutdown();
    crate::plugins::test_roots::remove_root(&root);
}

// Vite's order: `vite`'s cli.js statically imports the chunk its index.js
// re-exports, so vite evaluates at CLI start with the launching process's
// NODE_ENV; resolveConfig sets the default only later, right before
// loadConfigFromFile. The prewarmed host keeps that order: the config sees
// the default, vite's import did not.
#[tokio::test]
async fn a_prewarmed_vite_evaluates_before_the_node_env_default_like_vite() {
    let root = std::env::temp_dir().join(format!("oj-prewarm-order-{}", std::process::id()));
    crate::plugins::test_roots::remove_root(&root);
    let vite = root.join("node_modules/vite");
    std::fs::create_dir_all(&vite).unwrap();
    std::fs::write(
        vite.join("package.json"),
        r#"{"name":"vite","version":"8.0.0","type":"module","exports":{".":"./index.js"}}"#,
    )
    .unwrap();
    std::fs::write(
        vite.join("index.js"),
        r#"import { appendFileSync } from "node:fs";
const log = (line) => appendFileSync(new URL("./order.log", import.meta.url), line + "\n");
log("import " + (process.env.NODE_ENV ?? "unset"));
export async function loadConfigFromFile() {
  log("config " + (process.env.NODE_ENV ?? "unset"));
  return { config: { plugins: [] } };
}
"#,
    )
    .unwrap();
    let plugins = root.join("vite.config.mjs");
    std::fs::write(&plugins, "export default {};\n").unwrap();
    let config = serde_json::json!({
        "config": { "root": root.display().to_string() },
        "env": { "command": "serve", "mode": "development" },
        "pluginsFormat": "vite",
    })
    .to_string();
    let order = vite.join("order.log");

    let warm = PluginHost::prewarm(&root, None).expect("the engine starts ahead");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !order.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the warm import never ran"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let host = PluginHost::spawn_prewarmed(&root, &plugins, &config, warm)
        .await
        .expect("the host adopts the engine");
    tokio::time::timeout(std::time::Duration::from_secs(20), host.plugin_names())
        .await
        .expect("the host boots");

    let launched = std::env::var("NODE_ENV").ok();
    let at_import = launched.clone().unwrap_or_else(|| "unset".into());
    let at_config = launched.unwrap_or_else(|| "development".into());
    assert_eq!(
        std::fs::read_to_string(&order).unwrap(),
        format!("import {at_import}\nconfig {at_config}\n"),
    );
    host.shutdown();
    crate::plugins::test_roots::remove_root(&root);
}

#[test]
fn an_editor_hmr_gate_plugin_turns_the_native_gate_on_unless_oj_config_says_no() {
    let json = serde_json::json!({ "editorHmrGate": true });
    let mut config = oj_config::OjConfig::default();
    merge_vite_values(&mut config, parse_vite_values(&json));
    assert_eq!(config.server.as_ref().and_then(|s| s.hmr_gate), Some(true));

    // oj's own `server.hmrGate: false` wins.
    let mut config = oj_config::OjConfig::default();
    config.server.get_or_insert_with(Default::default).hmr_gate = Some(false);
    merge_vite_values(&mut config, parse_vite_values(&json));
    assert_eq!(config.server.as_ref().and_then(|s| s.hmr_gate), Some(false));

    // No plugin: the gate is left as it was.
    let mut config = oj_config::OjConfig::default();
    merge_vite_values(&mut config, parse_vite_values(&serde_json::json!({})));
    assert_eq!(config.server.as_ref().and_then(|s| s.hmr_gate), None);
}

// A codeSplitting group's `name` and `test` functions reach a Start bundle
// as `$fn` handles into the container's registry (startFacts), which the
// bundle reads once, when its container opens, and keeps for the life of
// the dev server. A revived engine reopens the container from the boot seed
// with an empty registry, so every chunking pass after the revive called a
// handle nothing held ("oj: no config function #0") and the client bundle
// failed until the server was restarted. The handles stay callable: the
// fresh engine derives the registry again from the same config.
#[tokio::test]
async fn config_function_handles_outlive_the_engine_that_handed_them_out() {
    let root = std::env::temp_dir().join(format!("oj-start-fn-revive-{}", std::process::id()));
    crate::plugins::test_roots::remove_root(&root);
    std::fs::create_dir_all(&root).unwrap();
    let config_file = root.join("vite.config.mjs");
    std::fs::write(
        &config_file,
        r#"export default {
  plugins: [{
    name: "blocker",
    load(id) {
      if (id.includes("__block__")) for (;;) {}
      return null;
    },
  }],
  build: {
    rolldownOptions: {
      output: {
        codeSplitting: {
          groups: [{
            name: (id) => (id.includes("/vendor/") ? "vendor" : undefined),
            test: (id) => id.includes("node_modules"),
          }],
        },
      },
    },
  },
};
"#,
    )
    .unwrap();
    let config = serde_json::json!({
        "config": { "root": root.display().to_string(), "base": "/", "mode": "development", "command": "serve" },
        "env": { "command": "serve", "mode": "development" },
        "environment": { "name": "client", "mode": "dev" },
        "pluginsFormat": "vite",
        "ojStartMode": true,
        "ojBundleHost": true,
    })
    .to_string();
    let host = PluginHost::spawn_with_timeouts(
        &root,
        &config_file,
        &config,
        SpawnTimeouts {
            probe: Some(std::time::Duration::from_millis(500)),
            ..Default::default()
        },
    )
    .await
    .expect("the embedded engine spawns");
    // The dev client bundle's container, as start_bundle::hosts::open opens it.
    let client = host
        .open_environment_container("client", true, true)
        .await
        .expect("the client container opens");
    let facts = client.start_facts().await.expect("start facts are served");
    let group = &facts["codeSplitting"]["groups"][0];
    assert_eq!(group["name"], serde_json::json!({ "$fn": 0 }), "{facts}");
    assert_eq!(group["test"], serde_json::json!({ "$fn": 1 }), "{facts}");
    let ids = ["/app/vendor/a.js".to_string(), "/app/src/b.js".to_string()];
    let names = client
        .call_start_fn_batch(0, &ids, 1, None, true, true, ChunkPass::Start)
        .await
        .expect("the name function runs on the engine that handed it out");
    assert_eq!(
        names,
        vec![serde_json::json!("vendor"), serde_json::Value::Null]
    );

    // The engine dies under a wedged hook; the next call revives it.
    let err = host
        .load("__block__")
        .await
        .expect_err("a synchronous wedge silences the probes");
    assert!(
        err.contains("unresponsive") || err.contains("exited"),
        "the probe names the wedge: {err}"
    );
    clear_respawn_spacing(&host);

    let names = client
        .call_start_fn_batch(0, &ids, 2, None, true, true, ChunkPass::Start)
        .await
        .expect("the revived engine runs the name function behind the same handle");
    assert_eq!(
        names,
        vec![serde_json::json!("vendor"), serde_json::Value::Null]
    );
    let tests = client
        .call_start_fn_batch(
            1,
            &[
                "/app/node_modules/x.js".to_string(),
                "/app/src/b.js".to_string(),
            ],
            2,
            None,
            true,
            false,
            ChunkPass::Continue,
        )
        .await
        .expect("and the test function behind its handle");
    assert_eq!(
        tests,
        vec![serde_json::json!(true), serde_json::json!(false)]
    );
    host.shutdown();
    crate::plugins::test_roots::remove_root(&root);
}

/// A bundle host over `config_src` (a `vite.config.mjs`) with the dev client
/// bundle's container open, as start_bundle::hosts::open opens it. The config
/// may carry a `blocker` plugin whose `load` wedges the engine.
async fn start_fn_host(
    label: &str,
    config_src: &str,
) -> (
    StartFnRoot,
    std::sync::Arc<PluginHost>,
    std::sync::Arc<PluginHost>,
) {
    start_fn_host_as(label, config_src, true).await
}

/// The host's app root: the host is shut down and the root removed when the
/// test ends, a failed assertion included.
struct StartFnRoot(PathBuf, std::sync::Arc<PluginHost>);

impl Drop for StartFnRoot {
    fn drop(&mut self) {
        self.1.shutdown();
        crate::plugins::test_roots::remove_root(&self.0);
    }
}

/// [`start_fn_host`] over a host that is the bundle's own (`bundle_host`,
/// every container leaving oj's reimplemented plugins out) or the dev
/// server's, whose client bundle container alone leaves them out and reads
/// its facts from a config resolved for that (`leaveOutConfig`).
async fn start_fn_host_as(
    label: &str,
    config_src: &str,
    bundle_host: bool,
) -> (
    StartFnRoot,
    std::sync::Arc<PluginHost>,
    std::sync::Arc<PluginHost>,
) {
    start_fn_host_for(label, config_src, bundle_host, "serve", "client").await
}

/// [`start_fn_host_as`] for `command` ("serve", "build"), with the bundle
/// container of `environment` open.
async fn start_fn_host_for(
    label: &str,
    config_src: &str,
    bundle_host: bool,
    command: &str,
    environment: &str,
) -> (
    StartFnRoot,
    std::sync::Arc<PluginHost>,
    std::sync::Arc<PluginHost>,
) {
    let mode = if command == "build" {
        "production"
    } else {
        "development"
    };
    let root = std::env::temp_dir().join(format!("oj-start-fn-{label}-{}", std::process::id()));
    crate::plugins::test_roots::remove_root(&root);
    std::fs::create_dir_all(&root).unwrap();
    let config_file = root.join("vite.config.mjs");
    std::fs::write(&config_file, config_src).unwrap();
    let config = serde_json::json!({
        "config": { "root": root.display().to_string(), "base": "/", "mode": mode, "command": command },
        "env": { "command": command, "mode": mode },
        "environment": { "name": "client", "mode": if command == "build" { "build" } else { "dev" } },
        "pluginsFormat": "vite",
        "ojStartMode": true,
        "ojBundleHost": bundle_host,
    })
    .to_string();
    let host = PluginHost::spawn_with_timeouts(
        &root,
        &config_file,
        &config,
        SpawnTimeouts {
            probe: Some(std::time::Duration::from_millis(500)),
            ..Default::default()
        },
    )
    .await
    .expect("the embedded engine spawns");
    let client = host
        .open_environment_container(environment, true, true)
        .await
        .expect("the bundle's container opens");
    (
        StartFnRoot(root, std::sync::Arc::clone(&host)),
        host,
        client,
    )
}

/// Kills the engine under the `blocker` plugin's wedged `load`; the next
/// call revives it.
async fn wedge(host: &PluginHost) {
    let err = host
        .load("__block__")
        .await
        .expect_err("a synchronous wedge silences the probes");
    assert!(
        err.contains("unresponsive") || err.contains("exited"),
        "the probe names the wedge: {err}"
    );
    clear_respawn_spacing(host);
}

const BLOCKER: &str = r#"{
    name: "blocker",
    load(id) {
      if (id.includes("__block__")) for (;;) {}
      return null;
    },
  }"#;

/// The handle a fact carries.
fn fn_handle(fact: &serde_json::Value, facts: &serde_json::Value) -> u64 {
    fact["$fn"]
        .as_u64()
        .unwrap_or_else(|| panic!("{fact} is no handle: {facts}"))
}

// Every fact that carries a function hands it out as a handle, and each
// handle names the same function on a revived engine: an alias's
// `customResolver`, the `external` function, a group's `name` and `test`,
// and the worker bundle's `external`, which the facts reach last. The facts
// read again on the revived engine are the ones the bundle holds, handle for
// handle. On the bundle's own host and on the dev server's.
#[tokio::test]
async fn every_config_function_keeps_its_handle_across_a_revive() {
    for bundle_host in [true, false] {
        every_config_function_keeps_its_handle(bundle_host).await;
    }
}

async fn every_config_function_keeps_its_handle(bundle_host: bool) {
    let config = format!(
        r#"export default {{
  plugins: [{BLOCKER}],
  resolve: {{
    alias: [{{ find: "@app", replacement: "/src", customResolver: (id) => id + "?custom" }}],
  }},
  worker: {{ rolldownOptions: {{ external: (id) => id.startsWith("worker-external:") }} }},
  build: {{
    rolldownOptions: {{
      external: (id) => id.startsWith("external:"),
      output: {{
        codeSplitting: {{
          groups: [{{
            name: (id) => (id.includes("/vendor/") ? "vendor" : undefined),
            test: (id) => id.includes("node_modules"),
          }}],
        }},
      }},
    }},
  }},
}};
"#
    );
    let (_root, host, client) =
        start_fn_host_as(&format!("every-{bundle_host}"), &config, bundle_host).await;
    let facts = client.start_facts().await.expect("start facts are served");
    let resolver = fn_handle(&facts["alias"][0]["customResolver"], &facts);
    let external = fn_handle(&facts["external"], &facts);
    let name = fn_handle(&facts["codeSplitting"]["groups"][0]["name"], &facts);
    let test = fn_handle(&facts["codeSplitting"]["groups"][0]["test"], &facts);
    let worker = fn_handle(&facts["worker"]["external"], &facts);
    let mut all = vec![resolver, external, name, test, worker];
    all.sort_unstable();
    all.dedup();
    assert_eq!(all.len(), 5, "five functions, five handles: {facts}");
    assert!(worker > test, "the worker's is handed out last: {facts}");

    let ids = [
        "/app/vendor/a.js".to_string(),
        "/app/node_modules/b.js".to_string(),
    ];
    let answers = |generation: u64| {
        let client = client.clone();
        let ids = ids.clone();
        async move {
            (
                client
                    .call_start_fn(resolver, &serde_json::json!(["x"]))
                    .await,
                client
                    .call_start_fn(external, &serde_json::json!(["external:a", null, false]))
                    .await,
                client
                    .call_start_fn(external, &serde_json::json!(["./local", null, false]))
                    .await,
                client
                    .call_start_fn_batch(name, &ids, generation, None, true, true, ChunkPass::Start)
                    .await,
                client
                    .call_start_fn_batch(
                        test,
                        &ids,
                        generation,
                        None,
                        true,
                        false,
                        ChunkPass::Continue,
                    )
                    .await,
                client
                    .call_start_fn(worker, &serde_json::json!(["worker-external:a"]))
                    .await,
            )
        }
    };
    let before = answers(1).await;
    assert_eq!(
        before,
        (
            Ok(serde_json::json!("x?custom")),
            Ok(serde_json::json!(true)),
            Ok(serde_json::json!(false)),
            Ok(vec![serde_json::json!("vendor"), serde_json::Value::Null]),
            Ok(vec![serde_json::json!(false), serde_json::json!(true)]),
            Ok(serde_json::json!(true)),
        )
    );

    // The last handle first: the revived engine has to read every fact
    // before it can answer it.
    wedge(&host).await;
    assert_eq!(
        client
            .call_start_fn(worker, &serde_json::json!(["worker-external:a"]))
            .await,
        Ok(serde_json::json!(true)),
        "the handle the facts reach last, first on the revived engine"
    );
    assert_eq!(answers(2).await, before, "every handle after the revive");
    assert_eq!(
        client.start_facts().await.expect("the facts, read again"),
        facts,
        "the revived engine's facts are the ones the bundle holds"
    );
    assert_eq!(
        answers(3).await,
        before,
        "and after the facts were read again"
    );

    // A handle nothing handed out stays an error, on either engine.
    let missing = client.call_start_fn(99, &serde_json::json!([])).await;
    assert!(
        missing
            .as_ref()
            .is_err_and(|e| e.contains("no config function #99")),
        "{missing:?}"
    );
    wedge(&host).await;
    let missing = client
        .call_start_fn_batch(99, &ids, 4, None, true, true, ChunkPass::Start)
        .await;
    assert!(
        missing
            .as_ref()
            .is_err_and(|e| e.contains("no config function #99")),
        "{missing:?}"
    );
    assert_eq!(
        answers(5).await,
        before,
        "a missing handle cost the others nothing"
    );
}

// A chunking pass calls a group's `test` and `name`, and other calls come
// in beside them. On a revived engine they all arrive before the facts are
// read, and the read waits: the Worker environment's plugin list holds a
// thenable that takes its time every time it is awaited, with the group's
// functions handed out and the two after it not yet. Each call gets its
// function, the ones handed out after the wait included, and the facts read
// outright beside a call are the same facts.
#[tokio::test]
async fn calls_racing_on_a_revived_engine_each_get_their_function() {
    let config = format!(
        r#"const late = {{
  then(resolve) {{
    setTimeout(() => resolve({{ name: "late", _options: {{ pick: (x) => x + 1 }} }}), 120);
  }},
}};
export default {{
  plugins: [{BLOCKER}],
  environments: {{
    edge: {{
      resolve: {{ builtins: ["cloudflare:workers"] }},
      build: {{ rolldownOptions: {{ plugins: [late] }} }},
    }},
  }},
  worker: {{ rolldownOptions: {{ external: (id) => id.startsWith("worker-external:") }} }},
  build: {{
    rolldownOptions: {{
      output: {{
        codeSplitting: {{
          groups: [{{
            name: (id) => (id.includes("/vendor/") ? "vendor" : undefined),
            test: (id) => id.includes("node_modules"),
          }}],
        }},
      }},
    }},
  }},
}};
"#
    );
    let (_root, host, client) = start_fn_host("racing", &config).await;
    let facts = client.start_facts().await.expect("start facts are served");
    let name = fn_handle(&facts["codeSplitting"]["groups"][0]["name"], &facts);
    let test = fn_handle(&facts["codeSplitting"]["groups"][0]["test"], &facts);
    let pick = fn_handle(
        &facts["cloudflare"]["rolldownPlugins"][0]["options"]["pick"],
        &facts,
    );
    let worker = fn_handle(&facts["worker"]["external"], &facts);
    assert!(
        pick > test && worker > pick,
        "handed out after the wait: {facts}"
    );
    let ids = [
        "/app/vendor/a.js".to_string(),
        "/app/node_modules/b.js".to_string(),
    ];
    let named = Ok(vec![serde_json::json!("vendor"), serde_json::Value::Null]);
    for round in 0..3u64 {
        wedge(&host).await;
        let worker_args = serde_json::json!(["worker-external:a"]);
        let pick_args = serde_json::json!([41]);
        let (names, tests, external, names_again, picked, external_again) = tokio::join!(
            client.call_start_fn_batch(name, &ids, 10 + round, None, true, true, ChunkPass::Start),
            client.call_start_fn_batch(
                test,
                &ids,
                10 + round,
                None,
                true,
                false,
                ChunkPass::Continue
            ),
            client.call_start_fn(worker, &worker_args),
            client.call_start_fn_batch(name, &ids, 10 + round, None, true, true, ChunkPass::Start),
            client.call_start_fn(pick, &pick_args),
            client.call_start_fn(worker, &worker_args),
        );
        assert_eq!(names, named, "round {round}");
        assert_eq!(names_again, named, "round {round}");
        assert_eq!(
            tests,
            Ok(vec![serde_json::json!(false), serde_json::json!(true)]),
            "round {round}"
        );
        assert_eq!(external, Ok(serde_json::json!(true)), "round {round}");
        assert_eq!(external_again, Ok(serde_json::json!(true)), "round {round}");
        assert_eq!(picked, Ok(serde_json::json!(42)), "round {round}");
        // The facts read outright while calls come in.
        let (read, names, external) = tokio::join!(
            client.start_facts(),
            client.call_start_fn_batch(name, &ids, 20 + round, None, true, true, ChunkPass::Start),
            client.call_start_fn(worker, &worker_args),
        );
        assert_eq!(read.as_ref(), Ok(&facts), "round {round}");
        assert_eq!(names, named, "round {round}");
        assert_eq!(external, Ok(serde_json::json!(true)), "round {round}");
    }
}

// Which chunk policy a bundle follows, as rolldown's
// `bindingifyCodeSplitting` settles it from `output.codeSplitting`, the
// older `output.advancedChunks` and `output.inlineDynamicImports`: an object
// `codeSplitting` wins, `false` turns splitting off whatever the others say,
// `true` or nothing leaves it to `advancedChunks`, and
// `inlineDynamicImports` counts only where `codeSplitting` says nothing,
// where it turns splitting off.
#[tokio::test]
async fn the_code_splitting_fact_is_the_option_rolldown_would_follow() {
    let newer = r#"{ groups: [{ name: "newer", test: /newer/ }] }"#;
    let older = r#"{ groups: [{ name: "older", test: /older/ }] }"#;
    let group = |name: &str| serde_json::json!({ "groups": [{ "name": name, "test": { "$regex": name, "flags": "" } }] });
    let off = serde_json::json!(false);
    let output = |text: String| format!("rolldownOptions: {{ output: {{ {text} }} }}");
    let cases = [
        ("object-alone", output(format!("codeSplitting: {newer}")), group("newer")),
        ("older-alone", output(format!("advancedChunks: {older}")), group("older")),
        (
            "object-and-older",
            output(format!("codeSplitting: {newer}, advancedChunks: {older}")),
            group("newer"),
        ),
        (
            "true-and-older",
            output(format!("codeSplitting: true, advancedChunks: {older}")),
            group("older"),
        ),
        (
            "false-and-older",
            output(format!("codeSplitting: false, advancedChunks: {older}")),
            off.clone(),
        ),
        ("true-alone", output("codeSplitting: true".into()), serde_json::json!(true)),
        ("false-alone", output("codeSplitting: false".into()), off.clone()),
        ("neither", output(String::new()), serde_json::Value::Null),
        ("inline-alone", output("inlineDynamicImports: true".into()), off.clone()),
        (
            "inline-and-older",
            output(format!("inlineDynamicImports: true, advancedChunks: {older}")),
            off.clone(),
        ),
        (
            "inline-false-and-older",
            output(format!("inlineDynamicImports: false, advancedChunks: {older}")),
            group("older"),
        ),
        (
            "inline-beside-object",
            output(format!("inlineDynamicImports: true, codeSplitting: {newer}")),
            group("newer"),
        ),
        (
            "inline-beside-true",
            output(format!(
                "inlineDynamicImports: true, codeSplitting: true, advancedChunks: {older}"
            )),
            group("older"),
        ),
        // `output` as an array is a bundle per element under `vite build`;
        // the client bundle is the first, as Vite's bundled dev takes it.
        (
            "array",
            format!("rolldownOptions: {{ output: [{{ codeSplitting: {newer} }}, {{ codeSplitting: {older} }}] }}"),
            group("newer"),
        ),
        (
            "array-of-one-older",
            format!("rolldownOptions: {{ output: [{{ advancedChunks: {older} }}] }}"),
            group("older"),
        ),
        (
            "empty-array",
            "rolldownOptions: { output: [] }".to_string(),
            serde_json::Value::Null,
        ),
        // Vite's older name for the same options.
        (
            "rollup-options-array",
            format!("rollupOptions: {{ output: [{{ codeSplitting: {newer} }}] }}"),
            group("newer"),
        ),
    ];
    for (label, build, want) in cases {
        let config = format!("export default {{ build: {{ {build} }} }};\n");
        let (_root, _host, client) = start_fn_host(&format!("policy-{label}"), &config).await;
        let facts = client.start_facts().await.expect("start facts are served");
        assert_eq!(facts["codeSplitting"], want, "{label}: {facts}");
    }
}

// A group's `name` is called with the id and the chunking context, its
// `test` with the id alone, as rolldown calls each. The context answers
// from the module infos the pass sent. A `name` batch of a pass the engine
// holds nothing of, that does not start the pass over, is refused by name
// whether or not it carries infos of its own (a part of the graph the
// batches before it did not reach), so the pass starts over.
#[tokio::test]
async fn a_name_gets_the_chunking_context_a_test_the_id_alone() {
    let config = format!(
        r#"export default {{
  plugins: [{BLOCKER}],
  build: {{
    rolldownOptions: {{
      output: {{
        codeSplitting: {{
          groups: [{{
            name: (...args) => `${{args.length}}:${{typeof args[1]?.getModuleInfo}}:${{args[1].getModuleInfo(args[0])?.importers.join("+") ?? "none"}}`,
            test: (...args) => args.length === 1 && typeof args[0] === "string",
          }}],
        }},
      }},
    }},
  }},
}};
"#
    );
    let (_root, host, client) = start_fn_host("arguments", &config).await;
    let facts = client.start_facts().await.expect("start facts are served");
    let name = fn_handle(&facts["codeSplitting"]["groups"][0]["name"], &facts);
    let test = fn_handle(&facts["codeSplitting"]["groups"][0]["test"], &facts);
    let ids = ["/app/a.js".to_string(), "/app/b.js".to_string()];
    let a = serde_json::json!({ "id": "/app/a.js", "importers": ["/app/entry.js", "/app/b.js"] });
    let b = serde_json::json!({ "id": "/app/b.js", "importers": [] });
    let both = serde_json::json!([a, b]).to_string();
    let only_b = serde_json::json!([b]).to_string();
    let with_infos = Ok(vec![
        serde_json::json!("2:function:/app/entry.js+/app/b.js"),
        serde_json::json!("2:function:"),
    ]);
    let stale = |answer: &Result<Vec<serde_json::Value>, String>| {
        answer
            .as_ref()
            .is_err_and(|e| e.contains(STALE_CHUNK_INFOS))
    };
    let name_batch = |generation: u64, infos: Option<&str>, fresh: bool| {
        let (client, ids) = (client.clone(), ids.clone());
        let infos = infos.map(str::to_string);
        let pass = if fresh {
            ChunkPass::Start
        } else {
            ChunkPass::Continue
        };
        async move {
            client
                .call_start_fn_batch(name, &ids, generation, infos.as_deref(), true, true, pass)
                .await
        }
    };
    let tested = Ok(vec![serde_json::json!(true), serde_json::json!(true)]);
    let test_batch = || async {
        client
            .call_start_fn_batch(test, &ids, 0, None, true, false, ChunkPass::Continue)
            .await
    };

    // A test before any name of the pass: the id alone, and no claim on
    // the pass's infos.
    assert_eq!(test_batch().await, tested);
    // The pass arrives in two batches, the second with a part the first
    // did not reach.
    assert_eq!(
        name_batch(1, Some(&serde_json::json!([a]).to_string()), true).await,
        Ok(vec![
            serde_json::json!("2:function:/app/entry.js+/app/b.js"),
            serde_json::json!("2:function:none"),
        ])
    );
    assert_eq!(name_batch(1, Some(&only_b), false).await, with_infos);
    // Later batches of the pass come without infos and read the same ones.
    assert_eq!(name_batch(1, None, false).await, with_infos);
    // A pass with no info to send starts all the same.
    assert_eq!(
        name_batch(2, None, true).await,
        Ok(vec![
            serde_json::json!("2:function:none"),
            serde_json::json!("2:function:none")
        ])
    );
    // A pass the engine holds nothing of, not started: with no infos, and
    // with some.
    assert!(stale(&name_batch(3, None, false).await));
    assert!(stale(&name_batch(3, Some(&only_b), false).await));
    // Starting a pass over drops what the engine held of it.
    assert_eq!(name_batch(4, Some(&both), true).await, with_infos);
    assert_eq!(
        name_batch(4, Some(&only_b), true).await,
        Ok(vec![
            serde_json::json!("2:function:none"),
            serde_json::json!("2:function:"),
        ])
    );

    // A revive in the middle of pass 5. The test in between leaves the
    // pass to the name; a name batch carrying only the part the pass had
    // not sent yet is refused, as one carrying nothing is, and the pass
    // answers once it starts over.
    assert_eq!(
        name_batch(5, Some(&serde_json::json!([a]).to_string()), true)
            .await
            .map(|out| out.len()),
        Ok(2)
    );
    wedge(&host).await;
    assert_eq!(test_batch().await, tested);
    let refused = name_batch(5, Some(&only_b), false).await;
    assert!(
        stale(&refused),
        "a part of the pass is not the pass: {refused:?}"
    );
    let refused = name_batch(5, None, false).await;
    assert!(stale(&refused), "{refused:?}");
    assert_eq!(name_batch(5, Some(&both), true).await, with_infos);
    assert_eq!(name_batch(5, None, false).await, with_infos);
}

// `output.manualChunks` as a function is one group whose name it gives,
// called with the id and `getModuleInfo`, when no `codeSplitting` or
// `advancedChunks` object is the policy (rolldown's bindingifyCodeSplitting
// ignores it beside one).
#[tokio::test]
async fn a_manual_chunks_function_is_the_group_rolldown_makes_of_it() {
    let manual = r#"manualChunks: (id, { getModuleInfo }) => (id.includes("/vendor/") ? `vendor-${getModuleInfo(id)?.importers.length ?? "x"}` : undefined)"#;
    let object = r#"{ groups: [{ name: "own", test: /own/ }] }"#;
    let own = serde_json::json!({ "groups": [{ "name": "own", "test": { "$regex": "own", "flags": "" } }] });
    let made = serde_json::json!({ "groups": [{ "name": { "$fn": 0 } }] });
    let cases = [
        ("alone", manual.to_string(), made.clone()),
        (
            "beside-true",
            format!("codeSplitting: true, {manual}"),
            made.clone(),
        ),
        (
            "beside-true-and-inline",
            format!("codeSplitting: true, inlineDynamicImports: true, {manual}"),
            made.clone(),
        ),
        ("in-an-array", format!("__array__{manual}"), made.clone()),
        (
            "beside-object",
            format!("codeSplitting: {object}, {manual}"),
            own.clone(),
        ),
        (
            "beside-older",
            format!("advancedChunks: {object}, {manual}"),
            own,
        ),
        (
            "not-a-function",
            r#"manualChunks: { vendor: ["react"] }"#.to_string(),
            serde_json::Value::Null,
        ),
    ];
    for (label, output, want) in cases {
        let output = match output.strip_prefix("__array__") {
            Some(first) => format!("[{{ {first} }}, {{}}]"),
            None => format!("{{ {output} }}"),
        };
        let config = format!(
            "export default {{ plugins: [{BLOCKER}], build: {{ rolldownOptions: {{ output: {output} }} }} }};\n"
        );
        let (_root, host, client) = start_fn_host(&format!("manual-{label}"), &config).await;
        let facts = client.start_facts().await.expect("start facts are served");
        assert_eq!(facts["codeSplitting"], want, "{label}: {facts}");
        if want == made {
            let ids = ["/app/vendor/a.js".to_string(), "/app/src/b.js".to_string()];
            let infos =
                serde_json::json!([{ "id": "/app/vendor/a.js", "importers": ["/x", "/y"] }])
                    .to_string();
            let named = Ok(vec![serde_json::json!("vendor-2"), serde_json::Value::Null]);
            assert_eq!(
                client
                    .call_start_fn_batch(0, &ids, 1, Some(&infos), true, true, ChunkPass::Start)
                    .await,
                named,
                "{label}"
            );
            // And behind the same handle on a revived engine.
            wedge(&host).await;
            assert_eq!(
                client
                    .call_start_fn_batch(0, &ids, 2, Some(&infos), true, true, ChunkPass::Start)
                    .await,
                named,
                "{label}, revived"
            );
        }
    }
}

// What rolldown refuses of `manualChunks` (beside `codeSplitting: false` or
// `inlineDynamicImports: true`, or as Rollup's object form) a build refuses
// in rolldown's words, where the bundle reads its facts and again behind a
// handle. A dev server reads the same config without a word, as Vite's
// does unless it bundles: nothing is split, or the object is passed over.
// So does the bundle of an environment that only inherits the top level's
// `manualChunks` beside its own `inlineDynamicImports`.
#[tokio::test]
async fn manual_chunks_rolldown_refuses_fails_a_build_and_no_dev_server() {
    let manual = r#"manualChunks: (id) => (id.includes("/vendor/") ? "vendor" : undefined)"#;
    let no_splitting =
        r#""output.manualChunks" cannot be used when "output.codeSplitting" is set to false"#;
    let inlined = r#"Invalid value "true" for option "output.inlineDynamicImports" - this option is not supported for "output.manualChunks""#;
    let off = serde_json::json!(false);
    let cases = [
        (
            "no-splitting",
            format!("output: {{ codeSplitting: false, {manual} }}"),
            String::new(),
            "client",
            no_splitting,
            off.clone(),
        ),
        (
            "inlined",
            format!("output: {{ inlineDynamicImports: true, {manual} }}"),
            String::new(),
            "client",
            inlined,
            off.clone(),
        ),
        (
            "an-object-and-no-splitting",
            r#"output: { codeSplitting: false, manualChunks: { vendor: ["react"] } }"#.to_string(),
            String::new(),
            "client",
            no_splitting,
            off.clone(),
        ),
        (
            "an-object",
            r#"output: { manualChunks: { vendor: ["react"] } }"#.to_string(),
            String::new(),
            "client",
            "manualChunks is not a function",
            serde_json::Value::Null,
        ),
        // The client's `manualChunks`, at the top level, reaches the Worker
        // environment's output too, beside what that environment set.
        (
            "inherited",
            format!("output: {{ {manual} }}"),
            "environments: { edge: { build: { rolldownOptions: { output: { inlineDynamicImports: true } } } } },"
                .to_string(),
            "edge",
            inlined,
            off.clone(),
        ),
    ];
    for (label, output, environments, environment, message, dev_fact) in cases {
        let config = format!(
            "export default {{ {environments} build: {{ rolldownOptions: {{ external: (id) => id === \"x\", {output} }} }} }};\n"
        );
        let (_root, _host, bundle) = start_fn_host_for(
            &format!("refused-build-{label}"),
            &config,
            true,
            "build",
            environment,
        )
        .await;
        let refused = bundle.start_facts().await;
        assert!(
            refused.as_ref().is_err_and(|e| e.contains(message)),
            "{label}: {refused:?}"
        );
        // A read that failed left no registry: the handle reads again, and
        // is refused the same way.
        let called = bundle
            .call_start_fn(0, &serde_json::json!(["x", null, false]))
            .await;
        assert!(
            called.as_ref().is_err_and(|e| e.contains(message)),
            "{label}: {called:?}"
        );

        let (_root, _host, bundle) = start_fn_host_for(
            &format!("refused-dev-{label}"),
            &config,
            true,
            "serve",
            environment,
        )
        .await;
        let facts = bundle
            .start_facts()
            .await
            .unwrap_or_else(|e| panic!("{label}: a dev server reads the config: {e}"));
        assert_eq!(facts["codeSplitting"], dev_fact, "{label}: {facts}");
        assert_eq!(
            bundle
                .call_start_fn(
                    fn_handle(&facts["external"], &facts),
                    &serde_json::json!(["x", null, false])
                )
                .await,
            Ok(serde_json::json!(true)),
            "{label}"
        );
    }
}

// `getModuleInfo` in a group's `name` answers as rolldown's does: the
// graph's facts the pass sent, with the module's `meta` and
// `moduleSideEffects` as the plugins' hooks left them, from `resolveId`,
// `load` and `transform`, merged into one `meta` the function may write to.
// A module the hooks saw and said nothing of has an empty `meta` and a null
// `moduleSideEffects`, as one no hook saw; a module the pass does not have
// is null.
#[tokio::test]
async fn get_module_info_carries_what_the_hooks_said_of_the_module() {
    let config = r#"export default {
  plugins: [{
    name: "tagger",
    resolveId(source) {
      if (source === "shared") {
        return { id: "/app/shared.js", moduleSideEffects: false, meta: { fromResolve: 1 } };
      }
      if (source === "bare") return "/app/bare.js";
      return null;
    },
    load(id) {
      if (id === "/app/tagged.js") {
        return { code: "export default 1", meta: { tagger: { bucket: "i18n" } }, moduleSideEffects: "no-treeshake" };
      }
      return null;
    },
    transform(code, id) {
      if (id === "/app/shared.js") return { code, meta: { fromTransform: 2 } };
      if (id === "/app/seen.js") return { code };
      return null;
    },
  }],
  build: {
    rolldownOptions: {
      output: {
        manualChunks(id, { getModuleInfo }) {
          const info = getModuleInfo(id);
          if (!info) return "unknown";
          info.meta.visits = (info.meta.visits ?? 0) + 1;
          return JSON.stringify([info.meta, info.moduleSideEffects, info.importers.length]);
        },
      },
    },
  },
};
"#;
    let (_root, _host, client) = start_fn_host("module-meta", config).await;
    let facts = client.start_facts().await.expect("start facts are served");
    let name = fn_handle(&facts["codeSplitting"]["groups"][0]["name"], &facts);
    assert_eq!(
        client.resolve_id("shared", "").await,
        Ok(Some("/app/shared.js".to_string()))
    );
    assert_eq!(
        client.resolve_id("bare", "").await,
        Ok(Some("/app/bare.js".to_string()))
    );
    client
        .load("/app/tagged.js")
        .await
        .expect("the tagger loads the module");
    for id in ["/app/shared.js", "/app/seen.js", "/app/silent.js"] {
        client
            .transform("export default 1", id, id)
            .await
            .unwrap_or_else(|e| panic!("{id} goes through transform: {e}"));
    }
    let ids: Vec<String> = [
        "/app/shared.js",
        "/app/tagged.js",
        "/app/seen.js",
        "/app/silent.js",
        "/app/bare.js",
        "/app/untouched.js",
        "/app/unsent.js",
    ]
    .map(str::to_string)
    .to_vec();
    let infos = serde_json::Value::Array(
        ids[..6]
            .iter()
            .map(|id| serde_json::json!({ "id": id, "importers": ["/app/entry.js"] }))
            .collect(),
    )
    .to_string();
    let said = |visits: u64| {
        let of = |meta: serde_json::Value, effects: serde_json::Value| {
            let mut meta = meta;
            meta["visits"] = visits.into();
            serde_json::json!(serde_json::json!([meta, effects, 1]).to_string())
        };
        vec![
            of(
                serde_json::json!({ "fromResolve": 1, "fromTransform": 2 }),
                false.into(),
            ),
            of(
                serde_json::json!({ "tagger": { "bucket": "i18n" } }),
                "no-treeshake".into(),
            ),
            // Through `transform`, which said nothing of them.
            of(serde_json::json!({}), serde_json::Value::Null),
            of(serde_json::json!({}), serde_json::Value::Null),
            // Resolved to a bare id, and seen by no hook at all.
            of(serde_json::json!({}), serde_json::Value::Null),
            of(serde_json::json!({}), serde_json::Value::Null),
            serde_json::json!("unknown"),
        ]
    };
    assert_eq!(
        client
            .call_start_fn_batch(name, &ids, 1, Some(&infos), true, true, ChunkPass::Start)
            .await,
        Ok(said(1))
    );
    // What the function wrote to `meta` is there the next time, in the
    // pass and in the one after.
    assert_eq!(
        client
            .call_start_fn_batch(name, &ids, 1, None, true, true, ChunkPass::Continue)
            .await,
        Ok(said(2))
    );
    assert_eq!(
        client
            .call_start_fn_batch(name, &ids, 2, Some(&infos), true, true, ChunkPass::Start)
            .await,
        Ok(said(3))
    );
}

// A bundle whose hooks ran on an engine since respawned has lost what they
// said of its modules. A pass started so says it when a function reads
// `meta` or `moduleSideEffects`, where an empty `meta` would have named the
// chunks as if no hook had spoken; a function that reads the graph alone is
// answered as ever. The next bundle's pass starts whole.
#[tokio::test]
async fn a_pass_whose_hooks_ran_on_a_dead_engine_says_so_when_meta_is_read() {
    let config = r#"export default {
  plugins: [{
    name: "tagger",
    load(id) {
      return id === "/app/a.js" ? { code: "export default 1", meta: { bucket: "i18n" } } : null;
    },
  }],
  build: {
    rolldownOptions: {
      output: {
        codeSplitting: {
          groups: [
            { name: (id, { getModuleInfo }) => getModuleInfo(id).meta.bucket ?? "none" },
            { name: (id, { getModuleInfo }) => String(getModuleInfo(id).moduleSideEffects) },
            { name: (id, { getModuleInfo }) => `graph-${getModuleInfo(id).importers.length}` },
            { name: (id, { getModuleInfo }) => Object.keys(getModuleInfo(id)).sort().join("+") },
          ],
        },
      },
    },
  },
};
"#;
    let (_root, _host, client) = start_fn_host("hooks-lost", config).await;
    let facts = client.start_facts().await.expect("start facts are served");
    let handle = |group: usize| fn_handle(&facts["codeSplitting"]["groups"][group]["name"], &facts);
    client
        .load("/app/a.js")
        .await
        .expect("the tagger loads the module");
    let ids = ["/app/a.js".to_string()];
    let infos = serde_json::json!([{ "id": "/app/a.js", "importers": ["/x", "/y"] }]).to_string();
    let batch = |group: usize, generation: u64, pass: ChunkPass, tolerant: bool| {
        let (client, ids, infos) = (client.clone(), ids.clone(), infos.clone());
        let handle = handle(group);
        async move {
            let infos = (pass != ChunkPass::Continue).then_some(infos);
            client
                .call_start_fn_batch(
                    handle,
                    &ids,
                    generation,
                    infos.as_deref(),
                    tolerant,
                    true,
                    pass,
                )
                .await
        }
    };
    let lost = "respawned during this bundle";
    let threw = |answer: Result<Vec<serde_json::Value>, String>| {
        answer.is_ok_and(|out| out[0]["$threw"].as_str().is_some_and(|m| m.contains(lost)))
    };

    assert_eq!(
        batch(0, 1, ChunkPass::Start, true).await,
        Ok(vec![serde_json::json!("i18n")])
    );
    // The pass of a bundle whose hooks the engine did not run.
    assert!(threw(batch(0, 2, ChunkPass::StartHooksLost, true).await));
    assert!(
        threw(batch(1, 2, ChunkPass::Continue, true).await),
        "for the pass"
    );
    assert_eq!(
        batch(2, 2, ChunkPass::Continue, true).await,
        Ok(vec![serde_json::json!("graph-2")])
    );
    assert_eq!(
        batch(3, 2, ChunkPass::Continue, true).await,
        Ok(vec![serde_json::json!(
            "id+importers+meta+moduleSideEffects"
        )]),
        "the info names what it cannot give"
    );
    // A build fails on it.
    let failed = batch(0, 3, ChunkPass::StartHooksLost, false).await;
    assert!(
        failed.as_ref().is_err_and(|e| e.contains(lost)),
        "{failed:?}"
    );
    // The next bundle's pass is whole again.
    assert_eq!(
        batch(0, 4, ChunkPass::Start, true).await,
        Ok(vec![serde_json::json!("i18n")])
    );
    assert_eq!(
        batch(1, 4, ChunkPass::Continue, true).await,
        Ok(vec![serde_json::json!("null")])
    );
}

// A build takes from a group's functions what rolldown takes, and fails in
// rolldown's words on the rest: a string or nothing from `name`, a boolean
// or nothing from `test`, and no promise of either. oj's dev takes a result
// as it comes.
#[tokio::test]
async fn a_build_takes_from_a_groups_functions_what_rolldown_takes() {
    let config = r#"export default {
  build: {
    rolldownOptions: {
      output: {
        codeSplitting: {
          groups: [
            { name: () => "vendor", test: () => true },
            { name: () => "", test: () => false },
            { name: () => null, test: () => null },
            { name: () => undefined, test: () => undefined },
            { name: () => 5, test: () => 1 },
            { name: () => true, test: () => "yes" },
            { name: () => ({}), test: () => 0 },
            { name: async () => "later", test: async () => true },
            { name: () => { throw new Error("boom"); }, test: () => "" },
          ],
        },
      },
    },
  },
};
"#;
    let (_root, _host, client) = start_fn_host("returns", config).await;
    let facts = client.start_facts().await.expect("start facts are served");
    let ids = ["/app/a.js".to_string()];
    let call = |group: usize, which: &'static str, tolerant: bool| {
        let (client, ids) = (client.clone(), ids.clone());
        let handle = fn_handle(&facts["codeSplitting"]["groups"][group][which], &facts);
        async move {
            let named = which == "name";
            let pass = if named {
                ChunkPass::Start
            } else {
                ChunkPass::Continue
            };
            client
                .call_start_fn_batch(handle, &ids, 1, None, tolerant, named, pass)
                .await
                .map(|out| out[0].clone())
        }
    };
    let null = serde_json::Value::Null;
    // (group, a build's name, a build's test): the value, or the type
    // rolldown names in its refusal.
    type Taken = Result<serde_json::Value, &'static str>;
    let builds: [(usize, Taken, Taken); 8] = [
        (0, Ok("vendor".into()), Ok(true.into())),
        (1, Ok("".into()), Ok(false.into())),
        (2, Ok(null.clone()), Ok(null.clone())),
        (3, Ok(null.clone()), Ok(null.clone())),
        (4, Err("number"), Err("number")),
        (5, Err("boolean"), Err("string")),
        (6, Err("object"), Err("number")),
        (7, Err("object"), Err("object")),
    ];
    for (group, name, test) in builds {
        for (which, expected, want) in [("name", "string", &name), ("test", "boolean", &test)] {
            let got = call(group, which, false).await;
            match want {
                Ok(value) => assert_eq!(got.as_ref(), Ok(value), "group {group} {which}"),
                Err(returned) => {
                    let message = format!(
                        "advancedChunks group {which} option: The function returned `{returned}`, but expected `{expected}`."
                    );
                    assert!(
                        got.as_ref().is_err_and(|e| e.contains(&message)),
                        "group {group} {which}: {got:?}"
                    );
                }
            }
        }
    }
    let thrown = call(8, "name", false).await;
    assert!(
        thrown.as_ref().is_err_and(|e| e.contains("boom")),
        "{thrown:?}"
    );
    let refused = call(8, "test", false).await;
    assert!(
        refused
            .as_ref()
            .is_err_and(|e| e.contains("returned `string`")),
        "an empty string is no boolean: {refused:?}"
    );

    // Dev: as it comes, a promise awaited, a throw named.
    assert_eq!(call(4, "name", true).await, Ok(5.into()));
    assert_eq!(call(5, "test", true).await, Ok("yes".into()));
    assert_eq!(call(7, "name", true).await, Ok("later".into()));
    assert_eq!(call(7, "test", true).await, Ok(true.into()));
    assert_eq!(
        call(8, "name", true).await,
        Ok(serde_json::json!({ "$threw": "boom" }))
    );
}

// An environment container whose opener could not use it is dropped for
// good: the engine holds it no longer, the handle answers no more hooks,
// and a respawned engine does not reopen it, while the containers in use
// come back. The host's own container is never dropped.
#[tokio::test]
async fn a_discarded_environment_container_is_gone_from_every_engine() {
    let config = format!(
        r#"export default {{
  plugins: [{BLOCKER}],
  build: {{ rolldownOptions: {{ external: (id) => id.startsWith("external:") }} }},
}};
"#
    );
    let (_root, host, client) = start_fn_host("discarded", &config).await;
    let facts = client.start_facts().await.expect("start facts are served");
    let external = fn_handle(&facts["external"], &facts);
    assert_eq!(host.environment_containers(), 1, "the client bundle's");

    let unused = host
        .open_environment_container("client", true, true)
        .await
        .expect("a second container opens");
    assert_eq!(host.environment_containers(), 2);
    assert_eq!(unused.start_facts().await.as_ref(), Ok(&facts));
    unused.discard_environment_container().await;
    assert_eq!(host.environment_containers(), 1);
    let gone = unused.start_facts().await;
    assert!(gone.is_err(), "the engine dropped it: {gone:?}");
    // Dropping it twice, or the host's own, drops nothing more.
    unused.discard_environment_container().await;
    host.discard_environment_container().await;
    assert_eq!(host.environment_containers(), 1);
    assert_eq!(
        host.load("/app/a.js").await,
        Ok(None),
        "the host's own answers"
    );
    assert_eq!(client.start_facts().await.as_ref(), Ok(&facts));

    // One discarded while no engine was up to drop it (a respawn in
    // progress may have opened it from a seed taken before): the engine
    // still holds it, and the next environment container to open has it
    // dropped. The state such a discard leaves is made here by hand, the
    // window being a few instructions wide.
    let late = host
        .open_environment_container("client", true, true)
        .await
        .expect("a third container opens");
    let late_id = late.container;
    host.core
        .environments
        .lock()
        .unwrap()
        .retain(|(spec, _)| spec["id"] != late_id);
    host.core.discarded.lock().unwrap().insert(late_id);
    assert_eq!(host.environment_containers(), 1);
    assert_eq!(
        late.start_facts().await.as_ref(),
        Ok(&facts),
        "the engine holds it yet"
    );
    let another = host
        .open_environment_container("client", true, true)
        .await
        .expect("a container opens");
    let gone = late.start_facts().await;
    assert!(gone.is_err(), "dropped when the next one opened: {gone:?}");
    assert!(host.core.discarded.lock().unwrap().is_empty());
    assert_eq!(another.start_facts().await.as_ref(), Ok(&facts));
    another.discard_environment_container().await;
    assert_eq!(host.environment_containers(), 1);

    // A host that is down keeps what was discarded for the engine to come,
    // and is not respawned for it.
    let err = host
        .load("__block__")
        .await
        .expect_err("a synchronous wedge silences the probes");
    assert!(
        err.contains("unresponsive") || err.contains("exited"),
        "{err}"
    );
    let dead = host.engine_generation();
    host.core.discarded.lock().unwrap().insert(late_id);
    another.discard_environment_container().await;
    assert_eq!(
        host.engine_generation(),
        dead,
        "no respawn to drop a container"
    );
    assert!(host.core.discarded.lock().unwrap().contains(&late_id));
    clear_respawn_spacing(&host);
    assert_eq!(
        client.start_facts().await.as_ref(),
        Ok(&facts),
        "the engine is back"
    );
    assert_eq!(host.engine_generation(), dead + 1);
    let third = host
        .open_environment_container("client", true, true)
        .await
        .expect("a container opens on the revived engine");
    assert!(host.core.discarded.lock().unwrap().is_empty());
    third.discard_environment_container().await;
    assert_eq!(host.environment_containers(), 1);

    // A respawned engine reopens the one in use, and not the others.
    wedge(&host).await;
    assert_eq!(
        client
            .call_start_fn(external, &serde_json::json!(["external:a", null, false]))
            .await,
        Ok(serde_json::json!(true))
    );
    let gone = unused.start_facts().await;
    assert!(gone.is_err(), "not reopened: {gone:?}");
    assert_eq!(host.environment_containers(), 1);
}

// A module has one `meta` object, as in rolldown: what a plugin writes to
// it through `this.getModuleInfo(id).meta`, or to the info `moduleParsed`
// hands it, is what a chunk group's `getModuleInfo` reads, and what
// `resolveId` said of the module is there for the plugin's hooks.
#[tokio::test]
async fn a_modules_meta_is_one_object_for_the_plugins_and_the_chunk_groups() {
    let config = r#"export default {
  plugins: [{
    name: "tagger",
    resolveId(source) {
      return source === "shared" ? { id: "/app/shared.js", meta: { fromResolve: 1 } } : null;
    },
    load(id) {
      return id === "/app/shared.js" ? { code: "export default 1" } : null;
    },
    transform(code, id) {
      if (id !== "/app/shared.js") return null;
      const info = this.getModuleInfo(id);
      info.meta.viaContext = Object.keys(info.meta).sort().join("+");
      return { code, meta: { fromTransform: 2 } };
    },
    moduleParsed(info) {
      info.meta.parsed = (info.meta.parsed ?? 0) + 1;
    },
  }],
  build: {
    rolldownOptions: {
      output: {
        manualChunks(id, { getModuleInfo }) {
          const { meta } = getModuleInfo(id);
          meta.named = true;
          return JSON.stringify(meta);
        },
      },
    },
  },
};
"#;
    let (_root, host, client) = start_fn_host("one-meta", config).await;
    let facts = client.start_facts().await.expect("start facts are served");
    let name = fn_handle(&facts["codeSplitting"]["groups"][0]["name"], &facts);
    let id = "/app/shared.js";
    assert_eq!(
        client.resolve_id("shared", "").await,
        Ok(Some(id.to_string()))
    );
    client.load(id).await.expect("the tagger loads the module");
    client
        .transform("export default 1", id, id)
        .await
        .expect("the tagger transforms the module");
    let ids = [id.to_string()];
    let infos = serde_json::json!([{ "id": id, "importers": [] }]).to_string();
    let named = client
        .call_start_fn_batch(name, &ids, 1, Some(&infos), true, true, ChunkPass::Start)
        .await
        .expect("the function names the chunk");
    let meta: serde_json::Value = serde_json::from_str(named[0].as_str().unwrap()).unwrap();
    assert_eq!(meta["fromResolve"], 1, "{meta}");
    assert_eq!(meta["fromTransform"], 2, "{meta}");
    assert_eq!(
        meta["viaContext"], "fromResolve",
        "the transform saw what resolveId said, and wrote beside it: {meta}"
    );
    assert_eq!(meta["named"], true, "{meta}");
    // What the group wrote is there for the plugins too: a second
    // transform reads it through the context.
    client
        .transform("export default 2", id, id)
        .await
        .expect("the tagger transforms the module again");
    let named = client
        .call_start_fn_batch(name, &ids, 2, Some(&infos), true, true, ChunkPass::Start)
        .await
        .expect("the function names the chunk");
    let meta: serde_json::Value = serde_json::from_str(named[0].as_str().unwrap()).unwrap();
    let seen = meta["viaContext"].as_str().unwrap_or_default();
    assert!(
        seen.contains("named") && seen.contains("fromTransform") && seen.contains("fromResolve"),
        "{meta}"
    );
    drop(host);
}

// A plugin that assigns `info.meta` a new object, where most write into the
// one they are handed, gives the module that content: the hooks' later word
// merges onto it, as in Vite's dev, and the chunk groups read the same.
// Here the assigning plugin runs before the one whose `transform` returns a
// newer value. The assigned object stood apart from the module's, the newer
// value reached the module's alone, and the assigned object's older one was
// merged back over it when the transform ended, for the plugins and the
// groups alike.
#[tokio::test]
async fn a_meta_a_plugin_assigns_whole_takes_the_hooks_later_word() {
    let config = r#"let version = 0;
export default {
  plugins: [
    {
      name: "assigner",
      load(id) {
        return id === "/app/a.js" ? { code: "export default 1" } : null;
      },
      transform(code, id) {
        if (id !== "/app/a.js") return null;
        const info = this.getModuleInfo(id);
        info.meta = { ...info.meta, mine: true, seen: info.meta.version ?? 0 };
        return null;
      },
    },
    {
      name: "versioned",
      transform(code, id) {
        if (id !== "/app/a.js") return null;
        version += 1;
        return { code, meta: { version } };
      },
    },
    {
      name: "reader",
      transform(code, id) {
        if (id !== "/app/a.js") return null;
        return { code, meta: { read: this.getModuleInfo(id).meta.version } };
      },
    },
  ],
  build: {
    rolldownOptions: {
      output: { manualChunks: (id, { getModuleInfo }) => JSON.stringify(getModuleInfo(id).meta) },
    },
  },
};
"#;
    let (_root, _host, client) = start_fn_host("assigned-meta", config).await;
    let facts = client.start_facts().await.expect("start facts are served");
    let name = fn_handle(&facts["codeSplitting"]["groups"][0]["name"], &facts);
    let id = "/app/a.js";
    let ids = [id.to_string()];
    let infos = serde_json::json!([{ "id": id, "importers": [] }]).to_string();
    client.load(id).await.expect("the module loads");
    for version in 1..=3u64 {
        client
            .transform("export default 1", id, id)
            .await
            .expect("the plugins transform the module");
        let named = client
            .call_start_fn_batch(
                name,
                &ids,
                version,
                Some(&infos),
                true,
                true,
                ChunkPass::Start,
            )
            .await
            .expect("the function names the chunk");
        let meta: serde_json::Value = serde_json::from_str(named[0].as_str().unwrap()).unwrap();
        assert_eq!(
            meta,
            serde_json::json!({
                "mine": true,
                // What the assigner found: the transform before's.
                "seen": version - 1,
                // What `versioned` said after it, for the group...
                "version": version,
                // ...and for the plugin after it.
                "read": version,
            }),
            "transform {version}"
        );
    }
}

// The engine a host counts is its last one until a call respawns it. What
// is about to run on a host found dead runs on the engine after, which
// `live_engine_generation` brings up and counts; on a live host it is the
// count as it stands.
#[tokio::test]
async fn a_dead_hosts_engine_is_counted_once_it_is_up_again() {
    let config = format!("export default {{ plugins: [{BLOCKER}] }};\n");
    let (_root, host, client) = start_fn_host("live-engine", &config).await;
    let first = host.engine_generation();
    assert_eq!(host.live_engine_generation().await, first, "a live host");
    wedge(&host).await;
    assert_eq!(
        host.engine_generation(),
        first,
        "dead, and not yet respawned"
    );
    assert_eq!(client.live_engine_generation().await, first + 1);
    assert_eq!(host.engine_generation(), first + 1);
    assert_eq!(host.live_engine_generation().await, first + 1);
    assert!(client.start_facts().await.is_ok());
    assert_eq!(
        host.engine_generation(),
        first + 1,
        "the same engine answers"
    );
}

// Closing a worker bundle's container on a host that is down asks nothing
// of it: the container died with the engine, and a call would respawn the
// host to close an id the new engine never opened, or gave to another
// bundle since.
#[tokio::test]
async fn closing_a_worker_container_never_respawns_a_dead_host() {
    let config = format!("export default {{ plugins: [{BLOCKER}] }};\n");
    let (_root, host, client) = start_fn_host("close-dead", &config).await;
    let worker = client
        .open_worker_container(&[])
        .await
        .expect("a worker container opens");
    let closed_live = client
        .open_worker_container(&[])
        .await
        .expect("a second worker container opens");
    assert_eq!(closed_live.close_container().await, Ok(()));
    assert!(
        closed_live.load("/app/a.js").await.is_err(),
        "closed on the live engine"
    );
    assert_eq!(worker.load("/app/a.js").await, Ok(None));

    let before = host.engine_generation();
    wedge(&host).await;
    assert_eq!(worker.close_container().await, Ok(()));
    assert_eq!(host.engine_generation(), before, "no respawn to close it");
    // The host comes back for the next thing asked of it, and the handle
    // is of an engine gone.
    assert!(client.start_facts().await.is_ok());
    assert_eq!(host.engine_generation(), before + 1);
    assert_eq!(worker.close_container().await, Ok(()));
    assert!(worker.load("/app/a.js").await.is_err(), "a stale handle");
}
