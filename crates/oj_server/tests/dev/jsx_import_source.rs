// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A vite.config `oxc.jsx.importSource` (what `@vitejs/plugin-react`'s
//! `jsxImportSource` becomes, e.g. for Emotion) drives the dev JSX runtime
//! import, and a file's own `@jsxImportSource` pragma still wins. Fake
//! runtime packages stand in for the real ones so resolution succeeds.
//! `oj_compiler`'s `jsx_import_source_from_config_and_pragma_comment` pins the
//! transform alone; this pins the vite.config adoption through the server.
//! The build half is oj's `jsx_import_source_build`.

use crate::{App, INDEX_HTML};

fn fake_runtime(app: &App, name: &str) {
    let pkg = format!(
        r#"{{"name":"{name}","version":"1.0.0","exports":{{"./jsx-runtime":"./jsx-runtime.js","./jsx-dev-runtime":"./jsx-dev-runtime.js"}}}}"#
    );
    let body = format!(
        "export const Fragment = Symbol(\"{name}\");\nexport const jsx = (t, p) => ({{ from: \"{name}\", t, p }});\n"
    );
    let runtime = format!("{body}export const jsxs = jsx;\n");
    let dev_runtime = format!("{body}export const jsxDEV = jsx;\n");
    let dir = format!("node_modules/{name}");
    let (manifest, rt, dev_rt) = (
        format!("{dir}/package.json"),
        format!("{dir}/jsx-runtime.js"),
        format!("{dir}/jsx-dev-runtime.js"),
    );
    app.write(&[
        (manifest.as_str(), pkg.as_str()),
        (rt.as_str(), runtime.as_str()),
        (dev_rt.as_str(), dev_runtime.as_str()),
    ]);
}

#[tokio::test(flavor = "multi_thread")]
async fn configured_import_source_drives_dev_jsx_and_pragma_wins() {
    let app = App::new(&[
        ("package.json", r#"{"name":"jsx-src","version":"1.0.0"}"#),
        (
            "vite.config.js",
            "export default { oxc: { jsx: { runtime: \"automatic\", importSource: \"@fake/emotion\" } } };\n",
        ),
        ("src/App.jsx", "export const App = () => <div>hi</div>;\n"),
        (
            "src/Pragma.jsx",
            "/** @jsxImportSource @fake/solid */\nexport const Pragma = () => <span>p</span>;\n",
        ),
        (
            "src/main.js",
            "import { App } from \"./App\";\nimport { Pragma } from \"./Pragma\";\nwindow.__APP = App().from; window.__PRAGMA = Pragma().from;\n",
        ),
        ("index.html", INDEX_HTML),
    ]);
    fake_runtime(&app, "@fake/emotion");
    fake_runtime(&app, "@fake/solid");
    let server = app.serve().await;

    let app_js = server.text("/src/App.jsx").await;
    assert!(
        app_js.contains("@fake/emotion/jsx-dev-runtime"),
        "dev App.jsx does not import the configured source:\n{app_js}"
    );
    assert!(
        !regex::Regex::new(r#"["/]react/jsx"#)
            .unwrap()
            .is_match(&app_js),
        "dev App.jsx still imports react's runtime:\n{app_js}"
    );
    let pragma_js = server.text("/src/Pragma.jsx").await;
    assert!(
        pragma_js.contains("@fake/solid/jsx-dev-runtime"),
        "dev pragma did not win:\n{pragma_js}"
    );
}
