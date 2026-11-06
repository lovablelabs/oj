// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The app's vite.config runs once per start, as Vite's createServer resolves
//! it once: a cold boot's extraction runs on the plugin host's engine and the
//! host takes it over, and a warm boot's host evaluates it itself.

use crate::App;

const INDEX_HTML: &str = r#"<!doctype html><html><head></head><body><script type="module" src="/src/main.js"></script></body></html>"#;

fn config(log: &std::path::Path) -> String {
    format!(
        r#"import {{ appendFileSync }} from "node:fs";
const log = {log:?};
appendFileSync(log, "eval\n");
export default {{
  plugins: [
    {{
      name: "count-config",
      config() {{
        appendFileSync(log, "config\n");
      }},
      transform(code, id) {{
        if (id.endsWith("/src/main.js")) return code.replace("__HOSTED__", JSON.stringify("hosted"));
      }},
    }},
  ],
}};
"#
    )
}

fn counts(log: &std::path::Path) -> (usize, usize) {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let n = |what: &str| text.lines().filter(|l| *l == what).count();
    (n("eval"), n("config"))
}

#[tokio::test(flavor = "multi_thread")]
async fn plugin_host_evaluates_the_config_once() {
    crate::require_fixture!("vite");
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"config-evals","private":true,"type":"module"}"#,
        ),
        ("index.html", INDEX_HTML),
        ("src/main.js", "window.__h = __HOSTED__;\n"),
    ]);
    // Outside the root, so the writes never reach the watcher.
    let log = app.root.with_extension("evals.log");
    app.write(&[("vite.config.ts", &config(&log))]);
    app.link_fixture("vite");

    {
        let server = app.serve().await;
        let main = server.text("/src/main.js").await;
        assert!(
            main.contains("\"hosted\""),
            "the plugin host did not run:\n{main}"
        );
    }
    // The extraction, which the plugin host took over.
    assert_eq!(
        counts(&log),
        (1, 1),
        "cold boot (evaluations, config hooks)"
    );

    std::fs::remove_file(&log).unwrap();
    let server = app.serve().await;
    let main = server.text("/src/main.js").await;
    assert!(
        main.contains("\"hosted\""),
        "the plugin host did not run:\n{main}"
    );
    // The extraction is cached: only the plugin host evaluates.
    assert_eq!(
        counts(&log),
        (1, 1),
        "warm boot (evaluations, config hooks)"
    );
}

/// Boot adopts the config from several tasks at once (the Start dev server
/// and its script inputs): one cold key is evaluated once, the rest read it.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_extractions_evaluate_once() {
    crate::require_fixture!("vite");
    let app = App::new(&[(
        "package.json",
        r#"{"name":"config-evals-concurrent","private":true,"type":"module"}"#,
    )]);
    let log = app.root.with_extension("evals.log");
    app.write(&[("vite.config.ts", &config(&log))]);
    app.link_fixture("vite");

    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let extractions: Vec<_> = (0..3)
        .map(|_| {
            let (root, barrier) = (app.root.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                oj_server::plugins::extract_vite_values(&root, None, "serve", "development")
                    .is_some()
            })
        })
        .collect();
    for e in extractions {
        assert!(e.join().unwrap(), "an extraction returned no values");
    }
    assert_eq!(counts(&log), (1, 1), "(evaluations, config hooks)");
}

/// Vite's resolve runs the config hooks on the host's own instances: the
/// plugin list a hook sees holds those objects, the plugins of a config
/// `plugins` list are the config's own, and the hooks the resolve hid
/// (configResolved here) are back for the host to run, once.
#[tokio::test(flavor = "multi_thread")]
async fn config_hooks_see_the_plugin_instances_the_host_runs() {
    crate::require_fixture!("vite");
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"config-identity","private":true,"type":"module"}"#,
        ),
        ("index.html", INDEX_HTML),
        ("src/main.js", "window.__seen = __SEEN__;\n"),
        (
            "vite.config.mjs",
            r#"const seen = { resolved: 0 };
const scan = { name: "scan-probe" };
const probe = {
  name: "identity-probe",
  config(config) {
    seen.self = config.plugins.flat(Infinity).includes(probe);
  },
  configResolved(config) {
    seen.resolved++;
    seen.scan = config.optimizeDeps.rolldownOptions?.plugins?.[0] === scan;
  },
  transform(code, id) {
    if (id.endsWith("/src/main.js")) return code.replace("__SEEN__", JSON.stringify(seen));
  },
};
export default { optimizeDeps: { rolldownOptions: { plugins: [scan] } }, plugins: [probe] };
"#,
        ),
    ]);
    app.link_fixture("vite");
    let server = app.serve().await;
    let main = server.text("/src/main.js").await;
    let compact: String = main.split_whitespace().collect();
    assert!(
        compact.contains(r#"{"resolved":1,"self":true,"scan":true}"#),
        "what the hooks saw:\n{main}"
    );
}

/// A config hook that throws inside Vite's resolve is not run again by the
/// host: a Start host skips it, as it skips any failing config hook.
#[tokio::test(flavor = "multi_thread")]
async fn a_throwing_config_hook_runs_once() {
    crate::require_fixture!("vite");
    let app = App::new(&[(
        "package.json",
        r#"{"name":"config-throws","private":true,"type":"module"}"#,
    )]);
    let log = app.root.with_extension("throws.log");
    app.write(&[(
        "vite.config.mjs",
        &format!(
            r#"import {{ appendFileSync }} from "node:fs";
export default {{
  plugins: [
    {{ name: "throws", config() {{ appendFileSync({log:?}, "config\n"); throw new Error("config failed"); }} }},
    {{ name: "after", transform() {{}} }},
  ],
}};
"#
        ),
    )]);
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
    .expect("the host starts");
    let names = host.plugin_names().await;
    host.shutdown();
    assert!(names.iter().any(|n| n == "after"), "{names:?}");
    assert_eq!(counts(&log), (0, 1), "(evaluations, config hooks)");
}

/// Callers waiting on an extraction take its outcome, a failed one too: a
/// config that throws is evaluated once, not once per waiting caller.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_extractions_share_a_failure() {
    crate::require_fixture!("vite");
    let app = App::new(&[(
        "package.json",
        r#"{"name":"config-evals-failing","private":true,"type":"module"}"#,
    )]);
    let log = app.root.with_extension("evals.log");
    app.write(&[(
        "vite.config.ts",
        &format!(
            "import {{ appendFileSync }} from \"node:fs\";\nappendFileSync({log:?}, \"eval\\n\");\n// Slow, so the waiting callers are all queued behind it.\nconst until = Date.now() + 300;\nwhile (Date.now() < until) {{}}\nthrow new Error(\"config failed\");\n"
        ),
    )]);
    app.link_fixture("vite");

    // How many evaluations one failing extraction makes.
    assert!(
        oj_server::plugins::extract_vite_values(&app.root, None, "serve", "development").is_none()
    );
    let one = counts(&log);
    std::fs::remove_file(&log).unwrap();

    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let extractions: Vec<_> = (0..3)
        .map(|_| {
            let (root, barrier) = (app.root.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                oj_server::plugins::extract_vite_values(&root, None, "serve", "development")
                    .is_some()
            })
        })
        .collect();
    for e in extractions {
        assert!(!e.join().unwrap(), "a failing config returned values");
    }
    assert_eq!(
        counts(&log),
        one,
        "(evaluations, config hooks) of three callers"
    );
}

/// `/src/main.js` served on a cold boot (the extraction's run, which the
/// plugin host takes over) and then a warm one (the host's own resolve):
/// what the config's plugins saw, which must not depend on the boot.
async fn cold_and_warm(app: &App) -> [String; 2] {
    let mut seen = Vec::new();
    for _ in 0..2 {
        let server = app.serve().await;
        let main = server.text("/src/main.js").await;
        seen.push(main.split_whitespace().collect::<String>());
    }
    [seen[0].clone(), seen[1].clone()]
}

fn seen_app(name: &str, config: &str) -> App {
    let app = App::new(&[
        (
            "package.json",
            &format!(r#"{{"name":"{name}","private":true,"type":"module"}}"#),
        ),
        ("index.html", INDEX_HTML),
        ("src/main.js", "window.__seen = __SEEN__;\n"),
        ("vite.config.mjs", config),
    ]);
    app.link_fixture("vite");
    app
}

/// A config hook sees a sibling's configResolved and may wrap it, as under
/// Vite, which hides nothing: the wrapper and the sibling's own hook run.
#[tokio::test(flavor = "multi_thread")]
async fn a_config_hook_wraps_a_siblings_config_resolved_on_any_boot() {
    crate::require_fixture!("vite");
    let app = seen_app(
        "config-wrap",
        r#"const seen = [];
const b = { name: "b", configResolved() { seen.push("b"); } };
const a = {
  name: "a",
  config() {
    const own = b.configResolved;
    seen.push(typeof own);
    b.configResolved = function (c) { seen.push("wrapper"); return own?.call(this, c); };
  },
  transform(code, id) {
    if (id.endsWith("/src/main.js")) return code.replace("__SEEN__", JSON.stringify(seen));
  },
};
export default { plugins: [a, b] };
"#,
    );
    for (boot, main) in ["cold", "warm"].iter().zip(cold_and_warm(&app).await) {
        assert!(
            main.contains(r#"["function","wrapper","b"]"#),
            "{boot} boot:\n{main}"
        );
    }
}

/// A package the config imports keeps what it took at import (process.env,
/// console) for the server's life: a cold boot's run loads it on the host's
/// own engine, so those are the host's, as the warm boot's are.
#[tokio::test(flavor = "multi_thread")]
async fn a_package_keeps_the_hosts_process_env_on_any_boot() {
    crate::require_fixture!("vite");
    let app = seen_app(
        "config-capture",
        "import capture from \"capture\";\nexport default { plugins: [capture()] };\n",
    );
    app.write(&[
        (
            "node_modules/capture/package.json",
            r#"{"name":"capture","type":"module","main":"index.js"}"#,
        ),
        (
            "node_modules/capture/index.js",
            r#"const env = process.env;
const log = console.log;
export default () => ({
  name: "capture",
  configureServer() { process.env.LATE = "1"; log("capture ready"); },
  transform(code, id) {
    if (id.endsWith("/src/main.js")) return code.replace("__SEEN__", JSON.stringify({ late: env.LATE ?? null, live: process.env.LATE ?? null }));
  },
});
"#,
        ),
    ]);
    for (boot, main) in ["cold", "warm"].iter().zip(cold_and_warm(&app).await) {
        assert!(
            main.contains(r#"{"late":"1","live":"1"}"#),
            "{boot} boot:\n{main}"
        );
    }
}

/// The resolved config's logging is the same whichever boot resolved it.
#[tokio::test(flavor = "multi_thread")]
async fn the_resolved_configs_logger_is_the_same_on_any_boot() {
    crate::require_fixture!("vite");
    let app = seen_app(
        "config-logger",
        r#"let seen = null;
export default { plugins: [{
  name: "logger-probe",
  configResolved(c) { seen = { level: c.logLevel, logger: typeof c.logger?.info }; },
  transform(code, id) {
    if (id.endsWith("/src/main.js")) return code.replace("__SEEN__", JSON.stringify(seen));
  },
}] };
"#,
    );
    let [cold, warm] = cold_and_warm(&app).await;
    assert_eq!(cold, warm, "cold vs warm boot");
    assert!(cold.contains(r#""logger":"function""#), "{cold}");
}

/// Start's router generator names the routes directory oj reads before any
/// host starts: an extraction on the plugin host's own engine reads it too
/// (`tanstackStart({ srcDirectory })`), as the generator runs there.
#[tokio::test(flavor = "multi_thread")]
async fn a_start_src_directory_is_read_by_an_extraction_the_host_takes_over() {
    crate::require_fixture!("vite", "@tanstack/react-start");
    let app = App::new(&[
        (
            "package.json",
            r#"{"name":"config-start-src","private":true,"type":"module"}"#,
        ),
        (
            "vite.config.ts",
            "import { tanstackStart } from \"@tanstack/react-start/plugin/vite\";\nimport { defineConfig } from \"vite\";\nexport default defineConfig({ plugins: [tanstackStart({ srcDirectory: \"app\" })] });\n",
        ),
        (
            "app/router.tsx",
            "import { createRouter } from \"@tanstack/react-router\";\nimport { routeTree } from \"./routeTree.gen\";\nexport function getRouter() { return createRouter({ routeTree }); }\n",
        ),
        (
            "app/routes/__root.tsx",
            "import { createRootRoute } from \"@tanstack/react-router\";\nexport const Route = createRootRoute({});\n",
        ),
    ]);
    let modules = crate::fixture("vite")
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    std::os::unix::fs::symlink(modules, app.root.join("node_modules")).unwrap();
    let warm = oj_server::plugins::PluginHost::prewarm(&app.root, None).expect("an engine");
    warm.offer_to_extraction(&app.root);
    let values = tokio::task::block_in_place(|| {
        oj_server::plugins::extract_vite_values(&app.root, None, "serve", "development")
    })
    .expect("the config's values");
    oj_server::plugins::withdraw_host_engine(&app.root);
    assert!(
        oj_server::plugins::host_engine_reusable(&app.root, "serve", "development"),
        "the run on the host's engine was kept"
    );
    let routes = values
        .start_router
        .map(|r| r.routes_directory)
        .expect("the router generator's paths");
    assert!(routes.ends_with("app/routes"), "{}", routes.display());
}
