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
