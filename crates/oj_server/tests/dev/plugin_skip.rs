// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! oj keeps no plugin host when the config has no plugins oj needs (e.g.
//! React-only, where oj does JSX in oxc): the host boots to report zero
//! active plugins, then is shut down, and the app, its pages included (Vite's
//! dev HTML transform, natively), is served without one.

use crate::App;

const INDEX: &str = r#"<!doctype html><html><head></head><body><script type="module" src="/src/main.js"></script></body></html>"#;

fn app(vite_config: &str) -> App {
    App::new(&[
        ("package.json", r#"{"name":"pskip","private":true}"#),
        ("index.html", INDEX),
        ("src/main.js", "document.body.dataset.ok = \"1\";\n"),
        ("vite.config.mjs", vite_config),
    ])
}

#[tokio::test(flavor = "multi_thread")]
async fn no_active_plugins_drops_the_host_and_serves_natively() {
    let app = app("export default { plugins: [] };\n");
    let server = app.serve().await;
    assert!(
        server.built.plugin_host.is_none(),
        "a zero-plugin config keeps no plugin host"
    );
    let home = server.text("/").await;
    assert!(home.contains("main.js"), "app not served:\n{home}");
    assert!(
        home.contains("<script type=\"module\" src=\"/@oj/client.js\"></script>"),
        "the page went through the dev HTML transform:\n{home}"
    );
    let main = server.text("/src/main.js").await;
    assert!(
        main.contains("dataset.ok"),
        "module served natively:\n{main}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_active_plugin_keeps_the_host() {
    let app = app(
        r#"export default { plugins: [{ name: "keep", transform(code, id) { return id.endsWith("main.js") ? code + "\nwindow.__KEPT = 1;\n" : null; } }] };
"#,
    );
    let server = app.serve().await;
    assert!(
        server.built.plugin_host.is_some(),
        "a config with a transform plugin keeps its host"
    );
    let main = server.text("/src/main.js").await;
    assert!(main.contains("__KEPT"), "the kept host transforms:\n{main}");
}
