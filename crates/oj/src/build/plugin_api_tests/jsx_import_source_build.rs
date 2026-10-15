// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The build half of the `oxc.jsx.importSource` check: the configured source
//! drives the JSX runtime import in the build, a file's `@jsxImportSource`
//! pragma still wins, and react's runtime is never referenced.

use super::{build_app, js_under, scratch, write_files};

fn fake_runtime(name: &str) -> [(String, String); 3] {
    let body = format!(
        "export const Fragment = Symbol(\"{name}\");\nexport const jsx = (t, p) => ({{ from: \"{name}\", t, p }});\n"
    );
    [
        (
            format!("node_modules/{name}/package.json"),
            format!(
                r#"{{"name":"{name}","version":"1.0.0","exports":{{"./jsx-runtime":"./jsx-runtime.js","./jsx-dev-runtime":"./jsx-dev-runtime.js"}}}}"#
            ),
        ),
        (
            format!("node_modules/{name}/jsx-runtime.js"),
            format!("{body}export const jsxs = jsx;\n"),
        ),
        (
            format!("node_modules/{name}/jsx-dev-runtime.js"),
            format!("{body}export const jsxDEV = jsx;\n"),
        ),
    ]
}

#[tokio::test]
async fn configured_import_source_drives_build_jsx_and_pragma_wins() {
    let root = scratch("jsx-import-source");
    write_files(
        &root,
        &[
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
            (
                "index.html",
                r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>"#,
            ),
        ],
    );
    for name in ["@fake/emotion", "@fake/solid"] {
        let files = fake_runtime(name);
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(p, c)| (p.as_str(), c.as_str()))
            .collect();
        write_files(&root, &refs);
    }
    build_app(&root).await.expect("the jsx app builds");

    let built = js_under(&root.join("dist/assets"));
    assert!(
        built.contains("@fake/emotion"),
        "build did not use the configured importSource:\n{built}"
    );
    assert!(
        built.contains("@fake/solid"),
        "build did not honor the file pragma:\n{built}"
    );
    assert!(
        !built.contains("react/jsx-runtime"),
        "build still references react's runtime:\n{built}"
    );
}
