// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The app's vite.config loads the way Node's Vite loads it. Under Deno (oj's
//! runtime) Vite writes the bundled config to `<config>.timestamp-*.mjs` at the
//! app root on every load; Node's Vite writes it under node_modules/.vite-temp
//! for an ESM config and runs a CJS config through its require hook.

use crate::App;

const INDEX_HTML: &str = r#"<!doctype html><html><head></head><body><script type="module" src="/src/main.js"></script></body></html>"#;

/// `__dirname` in the config is the case the bundled loader exists for: Vite
/// defines it from the source path, wherever the bundle is written.
const CONFIG: &str = r#"import path from "node:path";
export default { define: { __CONFIG_DIR__: JSON.stringify(path.basename(__dirname)) } };
"#;

fn app(package_json: &str) -> App {
    let app = App::new(&[
        ("package.json", package_json),
        ("index.html", INDEX_HTML),
        ("src/main.js", "window.__dir = __CONFIG_DIR__;\n"),
        ("vite.config.ts", CONFIG),
    ]);
    app.link_fixture("vite");
    app
}

fn config_temps_at_root(app: &App) -> Vec<String> {
    std::fs::read_dir(&app.root)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".timestamp-"))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn esm_config_bundles_into_vite_temp() {
    crate::require_fixture!("vite");
    let app = app(r#"{"name":"esm-config","private":true,"type":"module"}"#);
    let server = app.serve().await;

    let dir = app.root.file_name().unwrap().to_string_lossy().into_owned();
    let main = server.text("/src/main.js").await;
    assert!(
        main.contains(&format!("\"{dir}\"")),
        "config define (from __dirname) did not apply:\n{main}"
    );
    assert!(
        app.root.join("node_modules/.vite-temp").is_dir(),
        "the bundled config was not written under node_modules/.vite-temp"
    );
    assert_eq!(config_temps_at_root(&app), Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread")]
async fn cjs_config_loads_through_vite_require_hook() {
    crate::require_fixture!("vite");
    let app = app(r#"{"name":"cjs-config","private":true}"#);
    let server = app.serve().await;

    let dir = app.root.file_name().unwrap().to_string_lossy().into_owned();
    let main = server.text("/src/main.js").await;
    assert!(
        main.contains(&format!("\"{dir}\"")),
        "CJS config define (from __dirname) did not apply:\n{main}"
    );
    // Node's Vite evaluates a CJS config in memory: no temp file anywhere.
    assert!(!app.root.join("node_modules/.vite-temp").exists());
    assert_eq!(config_temps_at_root(&app), Vec::<String>::new());
}
