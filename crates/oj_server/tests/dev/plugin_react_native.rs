// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! @vitejs/plugin-react 6 is all work oj does natively: JSX and Fast Refresh
//! (react-babel, refresh-wrapper, config-post), the refresh runtime and
//! preamble (react-refresh, the virtual `@vitejs/plugin-react/preamble`), and
//! a bundled-dev-only preamble hook (refresh-fbm). So an app whose only
//! plugin is it keeps no plugin host, its pages take the native dev HTML
//! transform, and its preamble import is oj's own preamble.

use crate::{require_fixture, App};

#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_react_app_runs_without_a_plugin_host() {
    require_fixture!("@vitejs/plugin-react", "react", "react-dom", "vite");
    let app = App::new(&[
        ("package.json", r#"{"name":"react-native","version":"1.0.0","type":"module"}"#),
        (
            "vite.config.js",
            "import react from \"@vitejs/plugin-react\";\nexport default { plugins: [react()] };\n",
        ),
        (
            "index.html",
            "<!doctype html><html><head><title>r</title></head><body><div id=\"root\"></div><script type=\"module\" src=\"/src/main.jsx\"></script></body></html>",
        ),
        (
            "src/main.jsx",
            "import \"@vitejs/plugin-react/preamble\";\nimport { createRoot } from \"react-dom/client\";\nexport function App() { return <h1>hi</h1>; }\ncreateRoot(document.getElementById(\"root\")).render(<App />);\n",
        ),
    ]);
    // The whole fixture install: plugin-react 6 imports `vite/internal`.
    let node_modules = crate::fixture("react")
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    std::os::unix::fs::symlink(node_modules, app.root.join("node_modules")).unwrap();
    let server = app.serve().await;
    assert!(
        server.built.plugin_host.is_none(),
        "plugin-react alone leaves no active plugin"
    );
    let home = server.text("/").await;
    assert!(
        home.contains("<script type=\"module\" src=\"/@oj/refresh-preamble.js\"></script>"),
        "the page took the native dev HTML transform:\n{home}"
    );
    let main = server.text("/src/main.jsx").await;
    assert!(
        main.contains("import \"/@oj/refresh-preamble.js\""),
        "the virtual preamble is oj's:\n{main}"
    );
    assert!(
        main.contains("$RefreshReg$"),
        "Fast Refresh instruments the app:\n{main}"
    );
}
