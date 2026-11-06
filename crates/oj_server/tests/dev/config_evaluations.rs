// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The app's vite.config runs once per consumer, as Vite's createServer
//! resolves it once: the plugin host evaluates the file once and runs each
//! plugin's config hook once, and only a cold extraction adds an evaluation.

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
    // The extraction and the plugin host.
    assert_eq!(
        counts(&log),
        (2, 2),
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
