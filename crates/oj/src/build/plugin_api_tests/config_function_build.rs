// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The build half of the function-form `oj.config.js` check: the build calls
//! it with `command: "build"` and `mode: "production"`, so `base` is `/prod/`
//! in dist/index.html and the define carries `production`.

use std::fs;

use super::{build_app, scratch, write_files};

#[tokio::test]
async fn function_oj_config_sees_build_and_production() {
    let root = scratch("config-function-build");
    write_files(
        &root,
        &[
            ("package.json", r#"{"name":"fncfg-app","version":"1.0.0"}"#),
            (
                "oj.config.js",
                "export default ({ command, mode }) => ({\n  base: command === \"build\" ? \"/prod/\" : \"/dev/\",\n  define: { __MODE__: JSON.stringify(mode) },\n});\n",
            ),
            ("src/main.js", "window.__M = __MODE__;\n"),
            (
                "index.html",
                r#"<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>"#,
            ),
        ],
    );
    build_app(&root).await.expect("the app builds");

    let html = fs::read_to_string(root.join("dist/index.html")).unwrap();
    assert!(
        html.contains("src=\"/prod/"),
        "build config did not apply base=/prod/:\n{html}"
    );
    let assets = root.join("dist/assets");
    let main = fs::read_dir(&assets)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|f| f.starts_with("main-"))
        .expect("main chunk emitted");
    let code = fs::read_to_string(assets.join(main)).unwrap();
    assert!(
        code.contains("production"),
        "build config define did not apply mode=production:\n{code}"
    );
}
